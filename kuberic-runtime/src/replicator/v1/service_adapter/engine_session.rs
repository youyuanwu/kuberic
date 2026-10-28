//! Creation-owned transfer identity and admission. Native routing is not authority.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::task::Poll;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::protocol::public_operations::{
    RemoteReplicaSelection, ReplicaGoal, ReplicaPeerDescription, ReplicaPublication,
    ReplicaPublicationStage,
};
use crate::protocol::types::{
    ConfigurationDescriptor, Epoch, HistoryContext, ProcessSessionId, ReplicaIdentity, ReplicaRole,
    ReplicaStorageIncarnationContext, ReplicatorCreationId, ResourceUid,
};
use crate::{Result, RuntimeError};

const MAX_SESSIONS: usize = 256;

#[cfg(test)]
#[path = "engine_session_replacement_tests.rs"]
pub(super) mod replacement_tests;

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

pub(super) fn guid(domain: &[u8], bytes: &[u8]) -> [u8; 16] {
    let mut hash = Sha256::new();
    hash.update((domain.len() as u64).to_be_bytes());
    hash.update(domain);
    hash.update(bytes);
    let mut result = [0; 16];
    result.copy_from_slice(&hash.finalize()[..16]);
    result
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "WireBinding", into = "WireBinding")]
pub(in crate::replicator::v1) struct Binding {
    pub(super) nonce: String,
    pub(super) source: ReplicaPublication,
    pub(super) target: ReplicaPublication,
    pub(super) recovery: bool,
    pub(super) continuation: Option<String>,
    pub(super) reformation: bool,
    pub(super) live_target: Option<LiveTarget>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct LiveTarget {
    pub(super) predecessor: String,
    pub(super) previous: ConfigurationDescriptor,
    pub(super) source: Endpoint,
    pub(super) target: Endpoint,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct WireBinding {
    nonce: String,
    source: ReplicaPublication,
    target: ReplicaPublication,
    recovery: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    continuation: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    reformation: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    replacement: Option<ReplicaPeerDescription>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    live_target: Option<LiveTarget>,
}

impl From<Binding> for WireBinding {
    fn from(binding: Binding) -> Self {
        let replacement = binding.replacement();
        Self {
            nonce: binding.nonce,
            source: binding.source,
            target: binding.target,
            recovery: binding.recovery,
            continuation: binding.continuation,
            reformation: binding.reformation,
            replacement,
            live_target: binding.live_target,
        }
    }
}

impl TryFrom<WireBinding> for Binding {
    type Error = String;

    fn try_from(wire: WireBinding) -> std::result::Result<Self, Self::Error> {
        let binding = Self {
            nonce: wire.nonce,
            source: wire.source,
            target: wire.target,
            recovery: wire.recovery,
            continuation: wire.continuation,
            reformation: wire.reformation,
            live_target: wire.live_target,
        };
        if binding.replacement() != wire.replacement {
            return Err("replacement tag differs from the frozen exact predecessor".into());
        }
        binding
            .validate_structure()
            .map_err(|error| error.to_string())?;
        Ok(binding)
    }
}

impl Binding {
    pub(super) fn admitted_reconfiguration(&self) -> bool {
        !self.recovery
            && !self.original_member_idle()
            && (self.retained_receiver()
                || self.target.stage == ReplicaPublicationStage::AchievedScope
                    && self.target.role == ReplicaRole::ActiveSecondary)
            && self
                .source
                .scope
                .configuration
                .current
                .members
                .iter()
                .chain(
                    self.source
                        .scope
                        .configuration
                        .previous
                        .iter()
                        .flat_map(|previous| &previous.members),
                )
                .any(|member| {
                    member.identity == self.target.scope.storage.replica
                        && member.role == ReplicaRole::ActiveSecondary
                })
    }

    pub(super) fn new(source: ReplicaPublication, target: ReplicaPublication) -> Result<Self> {
        let binding = Self::candidate(source, target)?;
        binding.validate()?;
        Ok(binding)
    }

    pub(super) fn candidate(
        source: ReplicaPublication,
        target: ReplicaPublication,
    ) -> Result<Self> {
        let binding = Self {
            nonce: uuid::Uuid::new_v4().to_string(),
            source,
            target,
            recovery: false,
            continuation: None,
            reformation: false,
            live_target: None,
        };
        binding.validate_structure()?;
        Ok(binding)
    }

    pub(super) fn replacement(&self) -> Option<ReplicaPeerDescription> {
        self.source
            .scope
            .configuration
            .replicas
            .iter()
            .find(|peer| {
                peer.identity.replica_id == self.target.scope.storage.replica.replica_id
                    && peer.identity != self.target.scope.storage.replica
                    && self
                        .source
                        .scope
                        .configuration
                        .current
                        .members
                        .iter()
                        .any(|member| {
                            member.identity == peer.identity
                                && member.role == ReplicaRole::ActiveSecondary
                        })
            })
            .cloned()
    }

    pub(super) fn validate(&self) -> Result<()> {
        self.validate_structure()?;
        if self.original_member_idle() {
            let source = &self.source.scope;
            let target = &self.target.scope;
            if self.recovery
                || self.retained_receiver()
                || source.history != target.history
                || source.epoch != target.epoch
                || source.configuration.current != target.configuration.current
                || source.configuration.previous != target.configuration.previous
                || self.source.stage != ReplicaPublicationStage::AchievedScope
                || self.source.role != ReplicaRole::Primary
                || source.configuration.current.primary_id != source.storage.replica.replica_id
            {
                return Err(RuntimeError::AuthorityNotAdmitted);
            }
            return Ok(());
        }
        if let Some(old) = &self.live_target {
            old.source.validate()?;
            old.target.validate()?;
            let current = &self.source.scope.configuration.current;
            if !self.reformation
                || self.recovery
                || self.continuation.is_some()
                || old.predecessor == self.nonce
                || uuid::Uuid::parse_str(&old.predecessor).is_err()
                || old.source != self.primary()
                || old.target.identity != self.secondary().identity
                || old.target.storage != self.secondary().storage
                || old.target.process_session_id == self.secondary().process_session_id
                || old.target.creation_id == self.secondary().creation_id
                || old.previous.configuration_id != old.previous.expected_id()
                || self.source.scope.configuration.previous.as_ref() != Some(&old.previous)
                || self.target.scope.configuration.previous.as_ref() != Some(&old.previous)
                || current.primary_id != old.previous.primary_id
                || current.members != old.previous.members
                || current.write_quorum != old.previous.write_quorum
                || current.write_quorum <= 1
                || current.epoch.data_loss_number != old.previous.epoch.data_loss_number
                || current.epoch.configuration_number <= old.previous.epoch.configuration_number
            {
                return Err(RuntimeError::AuthorityNotAdmitted);
            }
        }
        if let Some(old) = &self.replacement() {
            let source = &self.source;
            let target = &self.target;
            let configuration = &source.scope.configuration;
            if self.recovery
                || self.reformation
                || self.continuation.is_some()
                || source.stage != ReplicaPublicationStage::AchievedScope
                || source.role != ReplicaRole::Primary
                || target.stage != ReplicaPublicationStage::AchievedScope
                || target.role != ReplicaRole::IdleSecondary
                || source.scope.history != target.scope.history
                || source.scope.epoch != target.scope.epoch
                || configuration.previous.is_some()
                || target.scope.configuration.previous.is_some()
                || configuration.current != target.scope.configuration.current
                || configuration.current.epoch != source.scope.epoch
                || configuration.current.write_quorum <= 1
                || configuration.current.primary_id != source.scope.storage.replica.replica_id
                || !configuration.replicas.contains(old)
                || old.identity.replica_id != target.scope.storage.replica.replica_id
                || old.identity == target.scope.storage.replica
                || old.storage_incarnation == target.scope.storage
                || old.process_session_id == target.scope.process_session_id
                || old.replication_address == target.replication_address
                || old.role != ReplicaRole::ActiveSecondary
                || old.is_up
                || old.is_ready
                || old.must_catchup
                || old.current_progress != crate::application::INVALID_LSN
                || old.catch_up_capability != crate::application::INVALID_LSN
                || !configuration.current.members.iter().any(|member| {
                    member.identity == old.identity && member.role == ReplicaRole::ActiveSecondary
                })
            {
                return Err(RuntimeError::AuthorityNotAdmitted);
            }
        }
        let overlaps_physical =
            self.source
                .scope
                .configuration
                .current
                .members
                .iter()
                .any(|member| {
                    member.identity.replica_id == self.target.scope.storage.replica.replica_id
                        && member.identity != self.target.scope.storage.replica
                });
        if overlaps_physical != self.replacement().is_some() {
            return Err(RuntimeError::AuthorityNotAdmitted);
        }
        if uuid::Uuid::parse_str(&self.nonce).is_err()
            || (self.reformation
                && (self.recovery
                    || self.continuation.is_some()
                    || !self.admitted_reconfiguration()))
            || self.continuation.as_ref().is_some_and(|predecessor| {
                self.recovery
                    || predecessor == &self.nonce
                    || uuid::Uuid::parse_str(predecessor).is_err()
                    || !self.admitted_reconfiguration()
            })
        {
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
            original_member: None,
            source: self.source.clone(),
            target: self.target.clone(),
            target_control_address: "engine-session".into(),
            generation: 1,
        }
        .validate()
        .map_err(|error| RuntimeError::AuthorityMismatch(error.into()))
    }

    pub(super) fn validate_structure(&self) -> Result<()> {
        self.source
            .validate()
            .and_then(|_| self.target.validate())
            .map_err(|error| RuntimeError::AuthorityMismatch(error.into()))?;
        self.primary().validate()?;
        self.secondary().validate()?;
        if uuid::Uuid::parse_str(&self.nonce).is_err()
            || self.primary() == self.secondary()
            || self.source.scope.history != self.target.scope.history
            || self
                .continuation
                .as_ref()
                .is_some_and(|old| uuid::Uuid::parse_str(old).is_err() || old == &self.nonce)
            || self.reformation && (self.recovery || self.continuation.is_some())
        {
            return Err(RuntimeError::AuthorityNotAdmitted);
        }
        Ok(())
    }

    pub(super) fn original_member_idle(&self) -> bool {
        self.target.stage == ReplicaPublicationStage::AchievedScope
            && self.target.role == ReplicaRole::IdleSecondary
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

    pub(super) fn primary(&self) -> Endpoint {
        Endpoint::publication(&self.source)
    }

    pub(super) fn retained_receiver(&self) -> bool {
        self.reformation || self.continuation.is_some()
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

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub(super) enum Control {
    Reserve,
    Activate,
    Confirm,
    Complete,
    Retire,
    CheckReformation,
    Reform,
    Recover,
    Recovered,
    Continue,
    CheckContinuation,
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
    RetiringSource,
    SourceRetired,
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

impl Entry {
    fn conflicts_with(&self, binding: &Binding) -> bool {
        self.binding.secondary().identity.replica_id == binding.secondary().identity.replica_id
            && self.phase != Phase::Retired
            && !(self.phase == Phase::SourceRetired
                && self.binding.secondary().storage != binding.secondary().storage)
    }
}

#[derive(Default)]
struct State {
    closed: bool,
    entries: BTreeMap<String, Entry>,
    receiver: Option<String>,
    builds: BTreeMap<String, super::build_provenance::CheckedBuild>,
    configuration_invocations: BTreeMap<
        (crate::protocol::types::OperationId, String),
        super::warm_source_transfer::ConfigurationInvocation,
    >,
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
    pub(super) fn retire(mut self, native_idle: bool, native_epoch: Epoch) -> Result<()> {
        let source_only = !native_idle
            && self
                .state
                .entries
                .get(&self.binding.nonce)
                .is_some_and(|entry| {
                    entry.binding == *self.binding
                        && entry.copy_completed
                        && entry.phase == Phase::Retiring
                        && self.binding.epoch().data_loss_number == native_epoch.data_loss_number
                        && self.binding.epoch().configuration_number
                            < native_epoch.configuration_number
                });
        if !native_idle
            && !source_only
            && !self
                .state
                .entries
                .get(&self.binding.nonce)
                .is_some_and(|entry| {
                    entry.binding == *self.binding
                        && ((!self.binding.recovery
                            && !entry.copy_completed
                            && matches!(entry.phase, Phase::Proposed | Phase::Active))
                            || (self.binding.retained_receiver()
                                && entry.copy_completed
                                && (entry.phase == Phase::Completed
                                    || entry.phase == Phase::Retiring
                                        && self.binding.epoch() == native_epoch)))
                })
        {
            return Err(RuntimeError::AuthorityNotAdmitted);
        }
        Sessions::retire_locked(&mut self.state, self.binding, || Ok(()))?;
        if source_only {
            self.state
                .entries
                .get_mut(&self.binding.nonce)
                .unwrap()
                .phase = Phase::RetiringSource;
        }
        Ok(())
    }
}

#[derive(Clone)]
pub(in crate::replicator::v1) struct Fence {
    admitted: Arc<AtomicBool>,
}

impl Fence {
    pub(super) fn denied() -> Self {
        Self {
            admitted: Arc::new(AtomicBool::new(false)),
        }
    }

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

pub(super) fn endpoint_matches_peer(endpoint: &Endpoint, peer: &ReplicaPeerDescription) -> bool {
    endpoint.identity == peer.identity
        && endpoint.storage == peer.storage_incarnation
        && endpoint.process_session_id == peer.process_session_id
        && endpoint.address == peer.replication_address
}

impl Sessions {
    pub(super) fn register_configuration(
        &self,
        invocation: super::warm_source_transfer::ConfigurationInvocation,
    ) -> Result<()> {
        let mut state = self.0.lock().unwrap();
        if state.closed {
            return Err(RuntimeError::Closed);
        }
        let key = (invocation.operation().clone(), invocation.call().to_owned());
        if let Some(old) = state.configuration_invocations.get(&key) {
            return if old == &invocation {
                Ok(())
            } else {
                Err(RuntimeError::AuthorityNotAdmitted)
            };
        }
        if state.configuration_invocations.len() >= MAX_SESSIONS {
            return Err(RuntimeError::QueueFull);
        }
        state.configuration_invocations.insert(key, invocation);
        Ok(())
    }

    pub(super) fn associate_configuration(&self, binding: &Binding) -> Result<()> {
        let state = self.0.lock().unwrap();
        if state.closed
            || !state
                .configuration_invocations
                .values()
                .any(|invocation| invocation.matches(binding, invocation.goal()))
            || !state
                .entries
                .get(&binding.nonce)
                .is_some_and(|entry| entry.binding == *binding && entry.proposed)
        {
            return Err(RuntimeError::AuthorityNotAdmitted);
        }
        Ok(())
    }

    pub(super) fn check_configuration(&self, binding: &Binding, goal: &ReplicaGoal) -> Result<()> {
        let state = self.0.lock().unwrap();
        if state.closed
            || !state
                .configuration_invocations
                .values()
                .any(|invocation| invocation.matches(binding, goal))
        {
            return Err(RuntimeError::AuthorityNotAdmitted);
        }
        Ok(())
    }

    pub(super) fn first_demotion_receiver(&self, binding: &Binding) -> Result<()> {
        let state = self.0.lock().unwrap();
        if state.closed
            || !binding.reformation
            || state.receiver.is_some()
            || state.entries.contains_key(&binding.nonce)
            || state.entries.values().any(|entry| {
                entry.binding.primary() != binding.secondary()
                    || !matches!(entry.phase, Phase::Retiring | Phase::Retired)
                    || entry.admitted.load(Ordering::Acquire)
            })
        {
            return Err(RuntimeError::AuthorityNotAdmitted);
        }
        Ok(())
    }

    pub(super) fn associate_demotion_receiver(
        &self,
        binding: &Binding,
        native: impl FnOnce() -> Result<()>,
    ) -> Result<()> {
        self.first_demotion_receiver(binding)?;
        let mut state = self.0.lock().unwrap();
        if state.closed
            || state.receiver.is_some()
            || state.entries.contains_key(&binding.nonce)
            || state.entries.len() >= MAX_SESSIONS
            || state
                .entries
                .values()
                .any(|entry| entry.phase != Phase::Retired)
        {
            return Err(RuntimeError::AuthorityNotAdmitted);
        }
        native()?;
        state.entries.insert(
            binding.nonce.clone(),
            Entry {
                binding: binding.clone(),
                phase: Phase::Reserved,
                admitted: Arc::new(AtomicBool::new(false)),
                copy_completed: true,
                predecessor: None,
                proposed: true,
            },
        );
        state.receiver = Some(binding.nonce.clone());
        Ok(())
    }
    pub(super) fn normal_predecessor(
        &self,
        binding: &Binding,
        checked: &super::warm_source_transfer::CheckedTransfer,
    ) -> Result<Binding> {
        let old = checked.predecessor(binding)?;
        let state = self.0.lock().unwrap();
        if state.closed
            || state.receiver.as_ref() != Some(&old.nonce)
            || state.entries.contains_key(&binding.nonce)
            || !state.entries.get(&old.nonce).is_some_and(|entry| {
                entry.binding == *old
                    && entry.copy_completed
                    && entry.phase == Phase::Completed
                    && entry.admitted.load(Ordering::Acquire)
            })
            || state
                .entries
                .values()
                .any(|entry| entry.binding.nonce != old.nonce && entry.conflicts_with(binding))
        {
            return Err(RuntimeError::AuthorityNotAdmitted);
        }
        Ok(old.clone())
    }

    pub(super) fn retire_normal_predecessor(
        &self,
        binding: &Binding,
        checked: &super::warm_source_transfer::CheckedTransfer,
    ) -> Result<()> {
        let old = self.normal_predecessor(binding, checked)?;
        let mut state = self.0.lock().unwrap();
        if state.closed
            || state.receiver.as_ref() != Some(&old.nonce)
            || !state.entries.get(&old.nonce).is_some_and(|entry| {
                entry.binding == old && entry.copy_completed && entry.phase == Phase::Completed
            })
            || state
                .entries
                .values()
                .any(|entry| entry.binding.nonce != old.nonce && entry.conflicts_with(binding))
        {
            return Err(RuntimeError::AuthorityNotAdmitted);
        }
        let entry = state.entries.get_mut(&old.nonce).unwrap();
        entry.admitted.store(false, Ordering::Release);
        entry.phase = Phase::Retiring;
        Ok(())
    }

    pub(super) fn reform_normal_receiver(
        &self,
        binding: &Binding,
        checked: &super::warm_source_transfer::CheckedTransfer,
        native: impl FnOnce() -> Result<()>,
    ) -> Result<()> {
        let old = checked.predecessor(binding)?;
        if old.epoch() != binding.epoch()
            || old.history() != binding.history()
            || old.secondary() != binding.secondary()
        {
            return Err(RuntimeError::AuthorityNotAdmitted);
        }
        self.replace_receiver(binding, old, native)
    }

    pub(super) fn partial_effect(&self, binding: &Binding) -> bool {
        self.0
            .lock()
            .unwrap()
            .entries
            .get(&binding.nonce)
            .is_some_and(|entry| {
                entry.binding == *binding
                    && matches!(
                        entry.phase,
                        Phase::Reserved | Phase::Active | Phase::Completed
                    )
            })
    }
    pub(super) fn all(&self) -> Vec<Binding> {
        self.0
            .lock()
            .unwrap()
            .entries
            .values()
            .map(|entry| entry.binding.clone())
            .collect()
    }
    pub(super) fn associate_build(
        &self,
        binding: &Binding,
        checked: &super::build_provenance::CheckedBuild,
    ) -> Result<()> {
        let mut state = self.0.lock().unwrap();
        if state.closed || checked.nonce()? != binding.nonce {
            return Err(RuntimeError::AuthorityNotAdmitted);
        }
        if let Some(old) = state.builds.get(&binding.nonce) {
            return if old == checked {
                Ok(())
            } else {
                Err(RuntimeError::AuthorityNotAdmitted)
            };
        }
        if state.builds.len() >= MAX_SESSIONS {
            return Err(RuntimeError::AuthorityNotAdmitted);
        }
        state.builds.insert(binding.nonce.clone(), checked.clone());
        Ok(())
    }

    pub(super) fn check_build(
        &self,
        binding: &Binding,
        checked: &super::build_provenance::CheckedBuild,
    ) -> Result<()> {
        if self.0.lock().unwrap().builds.get(&binding.nonce) != Some(checked) {
            return Err(RuntimeError::AuthorityNotAdmitted);
        }
        Ok(())
    }
    pub(super) fn live_target_proposal(&self, binding: Binding) -> Result<Binding> {
        binding.validate()?;
        let relation = binding
            .live_target
            .as_ref()
            .ok_or(RuntimeError::AuthorityNotAdmitted)?;
        let mut state = self.0.lock().unwrap();
        if state.closed {
            return Err(RuntimeError::Closed);
        }
        if let Some(entry) = state.entries.values().find(|entry| {
            entry.binding.live_target == binding.live_target
                && entry.binding.source == binding.source
                && entry.binding.target == binding.target
        }) {
            return Ok(entry.binding.clone());
        }
        if state.entries.contains_key(&binding.nonce)
            || state.entries.len() >= MAX_SESSIONS
            || !state
                .entries
                .get(&relation.predecessor)
                .is_some_and(|entry| {
                    entry.binding.primary() == relation.source
                        && entry.binding.secondary() == relation.target
                        && entry.binding.history() == binding.history()
                        && entry.binding.epoch().data_loss_number
                            == binding.epoch().data_loss_number
                        && entry.binding.epoch().configuration_number
                            < binding.epoch().configuration_number
                        && entry.copy_completed
                        && matches!(entry.phase, Phase::Completed | Phase::Retiring)
                })
            || state.entries.values().any(|entry| {
                entry.binding.nonce != relation.predecessor && entry.conflicts_with(&binding)
            })
        {
            return Err(RuntimeError::AuthorityNotAdmitted);
        }
        state.entries.insert(
            binding.nonce.clone(),
            Entry {
                binding: binding.clone(),
                phase: Phase::Recovering,
                admitted: Arc::new(AtomicBool::new(false)),
                copy_completed: false,
                predecessor: Some(relation.predecessor.clone()),
                proposed: false,
            },
        );
        Ok(binding)
    }

    pub(super) fn live_target_joined(&self, binding: &Binding) -> Result<()> {
        let mut state = self.0.lock().unwrap();
        let old = &binding
            .live_target
            .as_ref()
            .ok_or(RuntimeError::AuthorityNotAdmitted)?
            .predecessor;
        if state.closed
            || !state.entries.get(&binding.nonce).is_some_and(|entry| {
                entry.binding == *binding
                    && entry.phase == Phase::Recovering
                    && entry.predecessor.as_ref() == Some(old)
            })
            || !state.entries.get(old).is_some_and(|entry| {
                entry.phase == Phase::Retiring && !entry.admitted.load(Ordering::Acquire)
            })
        {
            return Err(RuntimeError::AuthorityNotAdmitted);
        }
        let predecessor = state.entries.get_mut(old).unwrap();
        predecessor.phase = Phase::Retired;
        predecessor.proposed = false;
        let entry = state.entries.get_mut(&binding.nonce).unwrap();
        entry.phase = Phase::Proposed;
        entry.proposed = true;
        Ok(())
    }

    pub(super) fn vacant_receiver(&self) -> bool {
        let state = self.0.lock().unwrap();
        !state.closed && state.receiver.is_none() && state.entries.is_empty()
    }

    pub(super) fn reformation_predecessor(&self, binding: &Binding) -> Result<Binding> {
        binding.validate()?;
        let state = self.0.lock().unwrap();
        let old = state
            .receiver
            .as_ref()
            .and_then(|nonce| state.entries.get(nonce))
            .filter(|entry| {
                let source = entry.binding.primary();
                let next = binding.primary();
                !state.closed
                    && binding.reformation
                    && entry.phase == Phase::Retiring
                    && entry.copy_completed
                    && entry.binding.secondary() == binding.secondary()
                    && entry.binding.history() == binding.history()
                    && entry.binding.epoch().data_loss_number == binding.epoch().data_loss_number
                    && entry.binding.epoch().configuration_number
                        < binding.epoch().configuration_number
                    && source != next
                    && (source.identity.replica_id != next.identity.replica_id
                        || (source.identity == next.identity
                            && source.storage == next.storage
                            && source.process_session_id != next.process_session_id
                            && source.creation_id != next.creation_id))
            })
            .ok_or(RuntimeError::AuthorityNotAdmitted)?;
        if state
            .entries
            .values()
            .any(|entry| entry.binding.nonce != old.binding.nonce && entry.conflicts_with(binding))
        {
            return Err(RuntimeError::AuthorityNotAdmitted);
        }
        Ok(old.binding.clone())
    }

    pub(super) fn reform_receiver(
        &self,
        binding: &Binding,
        native: impl FnOnce() -> Result<()>,
    ) -> Result<()> {
        let old = self.reformation_predecessor(binding)?;
        self.replace_receiver(binding, &old, native)
    }

    fn replace_receiver(
        &self,
        binding: &Binding,
        old: &Binding,
        native: impl FnOnce() -> Result<()>,
    ) -> Result<()> {
        let mut state = self.0.lock().unwrap();
        if state.closed
            || state.entries.len() >= MAX_SESSIONS
            || state.entries.contains_key(&binding.nonce)
            || state.receiver.as_ref() != Some(&old.nonce)
            || !state.entries.get(&old.nonce).is_some_and(|entry| {
                entry.binding == *old && entry.phase == Phase::Retiring && entry.copy_completed
            })
            || state
                .entries
                .values()
                .any(|entry| entry.binding.nonce != old.nonce && entry.conflicts_with(binding))
        {
            return Err(RuntimeError::AuthorityNotAdmitted);
        }
        native()?;
        state.entries.get_mut(&old.nonce).unwrap().phase = Phase::Retired;
        state.entries.insert(
            binding.nonce.clone(),
            Entry {
                binding: binding.clone(),
                phase: Phase::Reserved,
                admitted: Arc::new(AtomicBool::new(false)),
                copy_completed: true,
                predecessor: Some(old.nonce.clone()),
                proposed: true,
            },
        );
        state.receiver = Some(binding.nonce.clone());
        Ok(())
    }

    pub(super) fn retained_completed(&self, binding: &Binding) -> bool {
        let state = self.0.lock().unwrap();
        !state.closed
            && state.entries.get(&binding.nonce).is_some_and(|entry| {
                entry.binding == *binding
                    && entry.copy_completed
                    && matches!(entry.phase, Phase::Completed | Phase::Retiring)
            })
    }

    pub(super) fn continuation_predecessor(&self, binding: &Binding) -> Result<Binding> {
        binding.validate()?;
        let state = self.0.lock().unwrap();
        let old = binding
            .continuation
            .as_ref()
            .and_then(|nonce| state.entries.get(nonce))
            .filter(|entry| {
                !state.closed
                    && entry.phase == Phase::Retiring
                    && entry.copy_completed
                    && entry.binding.primary() == binding.primary()
                    && entry.binding.secondary() == binding.secondary()
                    && entry.binding.history() == binding.history()
                    && entry.binding.epoch().data_loss_number == binding.epoch().data_loss_number
                    && entry.binding.epoch().configuration_number
                        < binding.epoch().configuration_number
            })
            .ok_or(RuntimeError::AuthorityNotAdmitted)?;
        if state
            .entries
            .values()
            .any(|entry| entry.binding.nonce != old.binding.nonce && entry.conflicts_with(binding))
        {
            return Err(RuntimeError::AuthorityNotAdmitted);
        }
        Ok(old.binding.clone())
    }

    pub(super) fn continue_session(
        &self,
        binding: &Binding,
        receiver: bool,
        native: impl FnOnce() -> Result<()>,
    ) -> Result<()> {
        let old = self.continuation_predecessor(binding)?;
        let mut state = self.0.lock().unwrap();
        if state.closed
            || state.entries.len() >= MAX_SESSIONS
            || state.entries.contains_key(&binding.nonce)
            || state.entries.get(&old.nonce).map(|entry| entry.phase) != Some(Phase::Retiring)
            || (receiver && state.receiver.as_ref() != Some(&old.nonce))
            || state
                .entries
                .values()
                .any(|entry| entry.binding.nonce != old.nonce && entry.conflicts_with(binding))
        {
            return Err(RuntimeError::AuthorityNotAdmitted);
        }
        native()?;
        state.entries.get_mut(&old.nonce).unwrap().phase = Phase::Retired;
        state.entries.insert(
            binding.nonce.clone(),
            Entry {
                binding: binding.clone(),
                phase: if receiver {
                    Phase::Reserved
                } else {
                    Phase::Proposed
                },
                admitted: Arc::new(AtomicBool::new(false)),
                copy_completed: true,
                predecessor: Some(old.nonce),
                proposed: true,
            },
        );
        if receiver {
            state.receiver = Some(binding.nonce.clone());
        }
        Ok(())
    }

    pub(super) fn poll_delivery<T>(
        &self,
        origin: &mut Option<Binding>,
        continues: impl FnOnce(&T) -> bool,
        poll: impl FnOnce() -> Poll<T>,
    ) -> Poll<(T, Option<Binding>)> {
        let state = self.0.lock().unwrap();
        if origin.is_none() {
            *origin = state
                .receiver
                .as_ref()
                .and_then(|nonce| state.entries.get(nonce))
                .map(|entry| entry.binding.clone());
        }
        let Poll::Ready(value) = poll() else {
            return Poll::Pending;
        };
        let current = state
            .receiver
            .as_ref()
            .and_then(|nonce| state.entries.get(nonce))
            .filter(|entry| {
                !state.closed && matches!(entry.phase, Phase::Active | Phase::Completed)
            });
        let mut selected = origin.clone();
        if continues(&value)
            && let (Some(current), Some(origin)) = (current, origin)
        {
            let mut ancestor = Some(&current.binding);
            while let Some(binding) = ancestor {
                if binding == origin {
                    selected = Some(current.binding.clone());
                    break;
                }
                ancestor = state
                    .entries
                    .get(&binding.nonce)
                    .and_then(|entry| entry.predecessor.as_ref())
                    .and_then(|nonce| state.entries.get(nonce))
                    .map(|entry| &entry.binding);
            }
        }
        Poll::Ready((value, selected))
    }

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
        if binding.original_member_idle() && !state.builds.contains_key(&binding.nonce) {
            return Err(RuntimeError::AuthorityNotAdmitted);
        }
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
        if binding.retained_receiver()
            || state.entries.len() >= MAX_SESSIONS
            || (receiver && state.receiver.is_some())
            || state
                .entries
                .values()
                .any(|entry| entry.conflicts_with(binding))
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
        if binding.continuation.is_some() || binding.live_target.is_some() {
            return Err(RuntimeError::AuthorityNotAdmitted);
        }
        let mut state = self.0.lock().unwrap();
        if binding.original_member_idle() && !state.builds.contains_key(&binding.nonce) {
            return Err(RuntimeError::AuthorityNotAdmitted);
        }
        if state.closed {
            return Err(RuntimeError::Closed);
        }
        if let Some(entry) = state.entries.values().find(|entry| {
            (!state.builds.contains_key(&binding.nonce) || entry.binding.nonce == binding.nonce)
                && entry.binding.source.scope == binding.source.scope
                && entry.binding.target.scope == binding.target.scope
                && entry.binding.primary() == binding.primary()
                && entry.binding.secondary() == binding.secondary()
                && (matches!(
                    entry.phase,
                    Phase::Proposed | Phase::Active | Phase::Completed
                ) || (entry.phase == Phase::Recovering
                    && binding.replacement().is_some()
                    && entry.binding.replacement() == binding.replacement()))
        }) {
            return Ok(entry.binding.clone());
        }
        if binding.replacement().is_some() {
            return Self::replacement_reservation(&mut state, binding);
        }
        if state.entries.len() >= MAX_SESSIONS
            || state
                .entries
                .values()
                .any(|entry| entry.conflicts_with(&binding))
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

    fn replacement_reservation(state: &mut State, binding: Binding) -> Result<Binding> {
        let selected = binding.replacement().unwrap();
        let mut old = state.entries.values().filter(|entry| {
            endpoint_matches_peer(&entry.binding.secondary(), &selected)
                && entry.binding.primary() == binding.primary()
                && entry.binding.history() == binding.history()
                && entry.copy_completed
                && matches!(
                    entry.phase,
                    Phase::Completed | Phase::Retiring | Phase::SourceRetired
                )
        });
        let predecessor = old.next().map(|entry| entry.binding.clone());
        if old.next().is_some()
            || state.entries.len() >= MAX_SESSIONS
            || state.entries.values().any(|entry| {
                predecessor
                    .as_ref()
                    .is_none_or(|old| old.nonce != entry.binding.nonce)
                    && entry.conflicts_with(&binding)
            })
            || predecessor.as_ref().is_some_and(|old| {
                old.secondary().creation_id == binding.secondary().creation_id
                    || old.epoch().data_loss_number != binding.epoch().data_loss_number
                    || old.epoch().configuration_number > binding.epoch().configuration_number
            })
        {
            return Err(RuntimeError::AuthorityNotAdmitted);
        }
        state.entries.insert(
            binding.nonce.clone(),
            Entry {
                binding: binding.clone(),
                phase: Phase::Recovering,
                admitted: Arc::new(AtomicBool::new(false)),
                copy_completed: false,
                predecessor: predecessor.map(|old| old.nonce),
                proposed: false,
            },
        );
        Ok(binding)
    }

    pub(super) fn replacement_predecessor(&self, binding: &Binding) -> Result<Option<Binding>> {
        let state = self.0.lock().unwrap();
        let entry = state
            .entries
            .get(&binding.nonce)
            .filter(|entry| {
                !state.closed
                    && entry.binding == *binding
                    && binding.replacement().is_some()
                    && entry.phase == Phase::Recovering
            })
            .ok_or(RuntimeError::AuthorityNotAdmitted)?;
        Ok(entry
            .predecessor
            .as_ref()
            .map(|nonce| state.entries[nonce].binding.clone()))
    }

    /// The caller has joined only source carrier/ACK work, not the old receiver.
    pub(super) fn replacement_source_settled(&self, binding: &Binding) -> Result<()> {
        let mut state = self.0.lock().unwrap();
        let predecessor = state
            .entries
            .get(&binding.nonce)
            .filter(|entry| {
                !state.closed
                    && entry.binding == *binding
                    && binding.replacement().is_some()
                    && entry.phase == Phase::Recovering
            })
            .ok_or(RuntimeError::AuthorityNotAdmitted)?
            .predecessor
            .clone();
        if let Some(nonce) = predecessor {
            let old = state
                .entries
                .get_mut(&nonce)
                .ok_or(RuntimeError::AuthorityNotAdmitted)?;
            if !matches!(old.phase, Phase::Retiring | Phase::SourceRetired) {
                return Err(RuntimeError::AuthorityNotAdmitted);
            }
            old.admitted.store(false, Ordering::Release);
            old.phase = Phase::SourceRetired;
            old.proposed = false;
        }
        let entry = state.entries.get_mut(&binding.nonce).unwrap();
        entry.phase = Phase::Proposed;
        entry.proposed = true;
        Ok(())
    }

    pub(super) fn metadata_for_peer(&self, peer: &ReplicaPeerDescription) -> Option<Binding> {
        let state = self.0.lock().unwrap();
        let rank = |entry: &Entry| {
            let phase = match entry.phase {
                Phase::Proposed | Phase::Active | Phase::Completed => 3,
                Phase::SourceRetired => 2,
                Phase::Recovering | Phase::Reserved | Phase::Retiring | Phase::RetiringSource => 1,
                Phase::Retired => 0,
            };
            (entry.binding.epoch(), phase)
        };
        let best = state
            .entries
            .values()
            .filter(|entry| endpoint_matches_peer(&entry.binding.secondary(), peer))
            .map(&rank)
            .max()?;
        let mut selected = state.entries.values().filter(|entry| {
            endpoint_matches_peer(&entry.binding.secondary(), peer) && rank(entry) == best
        });
        let binding = selected.next()?.binding.clone();
        selected.next().is_none().then_some(binding)
    }

    pub(super) fn completed_replacement_for(&self, binding: &Binding) -> Option<Binding> {
        let state = self.0.lock().unwrap();
        if state.closed {
            return None;
        }
        let mut selected = state.entries.values().filter(|entry| {
            entry.phase == Phase::Completed
                && entry.copy_completed
                && entry.binding.replacement().as_ref().is_some_and(|old| {
                    old.identity.replica_id != binding.secondary().identity.replica_id
                })
                && entry.binding.primary() == binding.primary()
                && entry.binding.history() == binding.history()
                && entry.binding.epoch() == binding.epoch()
                && entry.binding.source.scope.configuration.current
                    == binding.source.scope.configuration.current
        });
        let binding = selected.next()?.binding.clone();
        selected.next().is_none().then_some(binding)
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
            || state
                .entries
                .values()
                .any(|entry| entry.binding.nonce != old.nonce && entry.conflicts_with(binding))
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
        let state = self.0.lock().unwrap();
        let mut selected = state.entries.values().filter(|entry| {
            entry.binding.secondary().identity.replica_id.value() == replica
                && !matches!(entry.phase, Phase::Retired | Phase::SourceRetired)
                && entry.proposed
        });
        let binding = selected.next()?.binding.clone();
        selected.next().is_none().then_some(binding)
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

    pub(super) fn retire_unavailable_receiver_credit(
        &self,
        binding: &Binding,
        credit: impl FnOnce() -> Result<()>,
    ) -> Result<Result<()>> {
        let mut state = self.0.lock().unwrap();
        if state.closed || state.receiver.as_ref() != Some(&binding.nonce) {
            return Err(RuntimeError::AuthorityNotAdmitted);
        }
        let entry = state
            .entries
            .get_mut(&binding.nonce)
            .filter(|entry| {
                entry.binding == *binding
                    && entry.copy_completed
                    && matches!(
                        entry.phase,
                        Phase::Active | Phase::Completed | Phase::Retiring
                    )
            })
            .ok_or(RuntimeError::AuthorityNotAdmitted)?;
        entry.phase = Phase::Retiring;
        entry.admitted.store(false, Ordering::Release);
        Ok(credit())
    }

    pub(super) fn drain_retiring_receiver_credit(
        &self,
        binding: &Binding,
        credit: impl FnOnce() -> Result<()>,
    ) -> Result<Result<()>> {
        let state = self.0.lock().unwrap();
        if state.closed
            || state.receiver.as_ref() != Some(&binding.nonce)
            || !state.entries.get(&binding.nonce).is_some_and(|entry| {
                entry.binding == *binding
                    && entry.phase == Phase::Retiring
                    && entry.copy_completed
                    && !entry.admitted.load(Ordering::Acquire)
            })
        {
            return Err(RuntimeError::AuthorityNotAdmitted);
        }
        Ok(credit())
    }

    pub(super) fn quarantine(&self, binding: &Binding, contain: impl FnOnce(bool)) {
        let mut state = self.0.lock().unwrap();
        let receiver = state.receiver.as_ref() == Some(&binding.nonce);
        let Some(entry) = state
            .entries
            .get_mut(&binding.nonce)
            .filter(|entry| entry.binding == *binding)
        else {
            return;
        };
        if !matches!(entry.phase, Phase::Active | Phase::Completed) {
            return;
        }
        entry.phase = Phase::Retiring;
        entry.admitted.store(false, Ordering::Release);
        contain(receiver);
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
        if !matches!(
            entry.phase,
            Phase::Retired | Phase::RetiringSource | Phase::SourceRetired
        ) {
            entry.phase = Phase::Retiring;
        }
        entry.admitted.store(false, Ordering::Release);
        Ok(taken)
    }

    /// Only an actual target join permits this transition, never an RPC timeout.
    pub(super) fn settled(&self, binding: &Binding) -> Result<()> {
        self.transition_retired(binding)
    }

    pub(super) fn source_retirement_selected(&self, binding: &Binding) -> bool {
        let state = self.0.lock().unwrap();
        state
            .entries
            .get(&binding.nonce)
            .is_some_and(|entry| entry.binding == *binding && entry.phase == Phase::RetiringSource)
    }

    pub(super) fn source_transport_settled(&self, binding: &Binding) -> Result<()> {
        let mut state = self.0.lock().unwrap();
        let entry = state
            .entries
            .get_mut(&binding.nonce)
            .filter(|entry| entry.binding == *binding && entry.phase == Phase::RetiringSource)
            .ok_or(RuntimeError::AuthorityNotAdmitted)?;
        entry.phase = Phase::SourceRetired;
        entry.proposed = false;
        Ok(())
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

    pub(super) fn retiring_receiver_successor(&self, origin: &Binding) -> Option<Binding> {
        let state = self.0.lock().unwrap();
        if state.closed {
            return None;
        }
        let receiver = state
            .receiver
            .as_ref()
            .and_then(|nonce| state.entries.get(nonce))?;
        if receiver.phase != Phase::Retiring
            || !receiver.copy_completed
            || receiver.binding.secondary() != origin.secondary()
            || receiver.binding.history() != origin.history()
        {
            return None;
        }
        let mut ancestor = Some(receiver);
        while let Some(entry) = ancestor {
            if entry.binding == *origin {
                return Some(receiver.binding.clone());
            }
            if !entry.binding.retained_receiver() {
                return None;
            }
            ancestor = entry
                .predecessor
                .as_ref()
                .and_then(|nonce| state.entries.get(nonce));
        }
        None
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
            if !matches!(
                entry.phase,
                Phase::Retired | Phase::SourceRetired | Phase::RetiringSource
            ) {
                entry.phase = Phase::Retiring;
            }
            entry.admitted.store(false, Ordering::Release);
        }
    }
}
