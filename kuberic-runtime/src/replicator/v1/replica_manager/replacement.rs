// Copyright (c) Microsoft Corporation. All rights reserved.
// Licensed under the MIT License. See LICENSES/service-fabric-MIT.txt.

use super::*;

pub(super) struct Replacement {
    pub(super) replica_id: i64,
    original: ReplicaSet,
    predecessor: ReplicationSession,
    successor: ReplicationSession,
    build: BuildIdleHandle,
    pub(super) joint: Option<ReplicaSet>,
    pub(super) caught_up: bool,
}

fn same_set(left: &ReplicaSet, right: &ReplicaSet) -> bool {
    left.write_quorum == right.write_quorum
        && left.members.len() == right.members.len()
        && left.members.iter().all(|member| {
            right.members.iter().any(|other| {
                member.replica_id == other.replica_id
                    && member.must_catchup == other.must_catchup
                    && member.session.same_instance(&other.session)
            })
        })
}

impl ReplicaManager {
    pub(in crate::replicator::v1) fn verify_replacement_source(
        &self,
        current: &ReplicaSet,
    ) -> Result<(), ManagerError> {
        self.ensure_active()?;
        self.check_reported_fault()?;
        if self.previous.is_some()
            || self.replacement.is_some()
            || !self.idle.is_empty()
            || !self.builds.is_empty()
            || self.catchup_wait.is_some()
            || !self
                .current
                .as_ref()
                .is_some_and(|actual| same_set(actual, current))
        {
            return Err(ConfigurationError::InvalidCopyTarget(self.local_replica_id).into());
        }
        Ok(())
    }

    pub(in crate::replicator::v1) fn contains_root(&self, session: &ReplicationSession) -> bool {
        self.session_roots()
            .iter()
            .any(|(_, known)| known.same_instance(session))
    }

    pub(in crate::replicator::v1) fn current_root(&self, id: i64) -> Option<ReplicationSession> {
        self.current
            .as_ref()?
            .members
            .iter()
            .find(|member| member.replica_id == id)
            .map(|member| member.session.clone())
    }

    pub(in crate::replicator::v1) fn can_restore_replacement_source(&self) -> bool {
        self.current.is_none()
            && self.previous.is_none()
            && self.pending.is_empty()
            && self.idle.is_empty()
            && self.builds.is_empty()
            && self.replacement.is_none()
    }

    pub(in crate::replicator::v1) fn restore_replacement_source(
        &mut self,
        configuration: EstablishedConfiguration,
        predecessor: &ReplicationSession,
    ) -> Result<ManagerEffects, ManagerError> {
        self.ensure_active()?;
        self.check_reported_fault()?;
        let unknown = predecessor.progress(self.queue.first_lsn_in_replication_queue() - 1);
        if !self.can_restore_replacement_source()
            || configuration.previous.is_some()
            || configuration.catchup_configuration
            || configuration.current.write_quorum <= 1
            || !Self::has_enough_replicas(&configuration.current)
            || predecessor.identity().remote_incarnation != 0
            || predecessor.transport_active()
            || unknown.received != -1
            || unknown.applied != -1
            || unknown.last_ack_at.is_some()
        {
            return Err(ConfigurationError::InvalidCopyTarget(self.local_replica_id).into());
        }
        let mut checked = BTreeMap::new();
        self.validate_set(&configuration.current, false, &mut checked)?;
        if !checked
            .values()
            .any(|session| session.same_instance(predecessor))
        {
            return Err(ConfigurationError::InvalidCopyTarget(self.local_replica_id).into());
        }
        let observation = self.observe_sets(Some(&configuration.current), None, &[])?;
        let mut effects = ManagerEffects::default();
        let hint = self.queue.first_lsn_in_replication_queue() - 1;
        for member in &configuration.current.members {
            if member.session.same_instance(predecessor) {
                continue;
            }
            let first = member.session.apply_lsn(hint).checked_add(1);
            let mut operations = Vec::new();
            if !first.is_some_and(|first| self.queue.get_operations(first, &mut operations)) {
                return Err(ConfigurationError::MissingCurrentReplay(member.replica_id).into());
            }
            if !operations.is_empty() {
                effects.sessions.push(SessionEffect::Replay {
                    session: member.session.clone(),
                    operations,
                    completed_hint: hint,
                });
            }
        }
        self.current = Some(configuration.current);
        self.queue.update_catchup_completion_lsn();
        self.advance(observation, &mut effects);
        Ok(effects)
    }

    pub(in crate::replicator::v1) fn admit_replacement(
        &mut self,
        predecessor: &ReplicationSession,
        member: EstablishedIdle,
        build: BuildIdleHandle,
        establishment: &mut PrimaryEstablishment,
    ) -> Result<(ManagerEffects, SessionSendBatch), ManagerError> {
        self.ensure_active()?;
        self.check_reported_fault()?;
        self.validate_session(member.replica_id, &member.session)?;
        let id = member.replica_id;
        let baseline = member
            .session
            .progress(self.queue.first_lsn_in_replication_queue() - 1);
        if !member.session.same_instance(&build.session())
            || baseline.received != 0
            || baseline.applied != 0
            || baseline.last_ack_at.is_some()
        {
            return Err(ConfigurationError::InvalidCopyTarget(id).into());
        }
        if self.previous.is_some()
            || self.replacement.is_some()
            || !self.idle.is_empty()
            || !self.builds.is_empty()
            || self.catchup_wait.is_some()
        {
            return Err(ConfigurationError::ReplacementPending(id).into());
        }
        let original = self
            .current
            .as_ref()
            .filter(|set| set.write_quorum > 1 && Self::has_enough_replicas(set))
            .ok_or(ConfigurationError::InvalidCopyTarget(id))?;
        if original.members.iter().any(|old| old.must_catchup)
            || !original.members.iter().any(|old| {
                old.replica_id == id && old.session.same_instance(predecessor) && !old.must_catchup
            })
            || original.members.iter().any(|old| {
                old.session.same_instance(&member.session)
                    || old.session.identity() == member.session.identity()
            })
            || member.session.identity().remote_incarnation <= 0
        {
            return Err(ConfigurationError::InvalidCopyTarget(id).into());
        }
        let Some((first, operations)) = self.queue.pending_for_build() else {
            return Err(ConfigurationError::MissingCurrentReplay(id).into());
        };
        let start = establishment
            .prepare_begin(first)
            .map_err(|error| ManagerError::BuildLifecycle(error.to_string()))?;
        self.replacement = Some(Replacement {
            replica_id: id,
            original: original.clone(),
            predecessor: predecessor.clone(),
            successor: member.session.clone(),
            build: build.clone(),
            joint: None,
            caught_up: false,
        });
        let mut effects = ManagerEffects::default();
        effects
            .sessions
            .push(SessionEffect::Close(predecessor.clone()));
        if !operations.is_empty() {
            effects.sessions.push(SessionEffect::Replay {
                session: member.session,
                operations,
                completed_hint: self.queue.first_lsn_in_replication_queue() - 1,
            });
        }
        // Not an idle voter or retained-storage recovery: only transfer traffic.
        self.builds.insert(id, build);
        Ok((effects, start))
    }

    pub(super) fn configure_replacement(
        &mut self,
        configuration: EstablishedConfiguration,
    ) -> Result<ManagerEffects, ManagerError> {
        let replacement = self.replacement.as_ref().unwrap();
        let id = replacement.replica_id;
        let final_cut = if let Some(joint) = &replacement.joint {
            if configuration
                .previous
                .as_ref()
                .is_some_and(|previous| same_set(previous, &replacement.original))
                && same_set(&configuration.current, joint)
                && configuration.catchup_configuration
            {
                // An exact callback retry must not move the captured boundary.
                return self.update_progress();
            }
            if configuration.previous.is_some()
                || configuration.catchup_configuration
                || !same_set(&configuration.current, joint)
                || !replacement.caught_up
            {
                return Err(ConfigurationError::ReplacementPending(id).into());
            }
            true
        } else {
            if configuration.previous.is_none()
                && same_set(&configuration.current, &replacement.original)
                && !configuration.catchup_configuration
            {
                return self.update_progress();
            }
            let mut expected = replacement.original.clone();
            let member = expected
                .members
                .iter_mut()
                .find(|member| member.replica_id == id)
                .unwrap();
            member.session = replacement.successor.clone();
            if !configuration.catchup_configuration
                || !configuration
                    .previous
                    .as_ref()
                    .is_some_and(|previous| same_set(previous, &replacement.original))
                || !same_set(&configuration.current, &expected)
                || !replacement
                    .build
                    .result()
                    .is_some_and(|end| end.result.is_ok())
                || replacement.successor.idle_faulted()
            {
                return Err(ConfigurationError::ReplacementPending(id).into());
            }
            false
        };
        // Each set still has one vote per logical ID. Only this selected pair
        // may differ between sets; every other root is exactly the old root.
        let mut checked = BTreeMap::new();
        self.validate_set(
            &configuration.current,
            configuration.catchup_configuration,
            &mut checked,
        )?;
        if let Some(previous) = &configuration.previous {
            checked.insert(id, replacement.predecessor.clone());
            self.validate_set(previous, false, &mut checked)?;
        }
        let observation = self.observe_sets(
            Some(&configuration.current),
            configuration.previous.as_ref(),
            &self.idle,
        )?;
        let mut effects = ManagerEffects::default();
        if final_cut {
            let replacement = self.replacement.take().unwrap();
            effects
                .sessions
                .push(SessionEffect::Close(replacement.predecessor));
            self.builds.remove(&id);
        } else {
            self.replacement.as_mut().unwrap().joint = Some(configuration.current.clone());
        }
        self.current = Some(configuration.current);
        self.previous = configuration.previous;
        self.catchup_configuration = configuration.catchup_configuration;
        self.queue.update_catchup_completion_lsn();
        self.advance(observation, &mut effects);
        Ok(effects)
    }
}
