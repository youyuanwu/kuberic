use std::collections::{BTreeMap, BTreeSet};

use kuberic_protocol::types::{
    ConfigurationDescriptor, ProcessSessionId, ReplicaIdentity, ReplicaRole, SecondaryRemovalStage,
    SecondaryRemovalWitness,
};
use kuberic_runtime_internal::transport::ReplicationAck;
use tokio::sync::oneshot;

use crate::application::Lsn;
use crate::authority::AdmittedAuthority;
use crate::{Result, RuntimeError};

#[derive(Debug, Default)]
pub struct QuorumTracker {
    authority: Option<AdmittedAuthority>,
    progress: BTreeMap<ReplicaIdentity, Lsn>,
    pending: BTreeMap<Lsn, Vec<oneshot::Sender<Result<Lsn>>>>,
    highest_lsn: Lsn,
    committed_lsn: Lsn,
    catch_up_boundary: Option<Lsn>,
    must_catch_up: BTreeSet<ReplicaIdentity>,
    sessions: BTreeMap<ReplicaIdentity, ProcessSessionId>,
    obsolete_sessions: BTreeSet<(ReplicaIdentity, ProcessSessionId)>,
    verified: BTreeMap<ReplicaIdentity, (u64, Lsn)>,
    witnesses: BTreeMap<ReplicaIdentity, SecondaryRemovalWitness>,
}

impl QuorumTracker {
    pub fn configure(&mut self, authority: AdmittedAuthority, local_progress: Lsn) -> Result<()> {
        authority.validate()?;
        let same_fence = self.authority.as_ref() == Some(&authority);
        if self.authority.is_some() && !same_fence {
            for (_, senders) in std::mem::take(&mut self.pending) {
                for sender in senders {
                    let _ = sender.send(Err(RuntimeError::AuthorityMismatch(
                        "authority changed before the write committed".to_string(),
                    )));
                }
            }
            self.progress.clear();
            self.verified.clear();
            self.witnesses.clear();
        }
        let members = authority_members(&authority);
        self.sessions
            .retain(|identity, _| members.contains(identity));
        self.progress
            .retain(|identity, _| members.contains(identity));
        self.progress
            .entry(authority.local_identity.clone())
            .and_modify(|progress| *progress = (*progress).max(local_progress))
            .or_insert(local_progress);
        self.highest_lsn = self.highest_lsn.max(local_progress);
        if !same_fence {
            self.catch_up_boundary = authority
                .secondary_removal
                .as_ref()
                .map(|e| e.preparation.boundary_lsn)
                .or_else(|| {
                    authority.previous_configuration.as_ref().map(|_| {
                        authority
                            .switchover_handoff
                            .as_ref()
                            .map_or(self.highest_lsn, |handoff| handoff.handoff_lsn)
                    })
                });
            self.must_catch_up = derive_must_catch_up(&authority);
        }
        self.authority = Some(authority);
        Ok(())
    }

    pub fn register_peer_session(
        &mut self,
        identity: ReplicaIdentity,
        session: ProcessSessionId,
    ) -> Result<()> {
        let authority = self
            .authority
            .as_ref()
            .ok_or(RuntimeError::AuthorityNotAdmitted)?;
        if session.is_empty()
            || identity == authority.local_identity
            || !authority.contains_member(&identity)
            || self
                .obsolete_sessions
                .contains(&(identity.clone(), session.clone()))
        {
            return Err(RuntimeError::AuthorityMismatch(
                "invalid peer session identity".into(),
            ));
        }
        if self.sessions.get(&identity) != Some(&session) {
            if let Some(previous) = self.sessions.get(&identity) {
                self.obsolete_sessions
                    .insert((identity.clone(), previous.clone()));
            }
            self.progress.remove(&identity);
            self.verified.remove(&identity);
            self.witnesses.remove(&identity);
            self.sessions.insert(identity, session);
        }
        Ok(())
    }

    pub fn record_verified_local_progress(&mut self, lsn: Lsn) {
        if let Some(authority) = &self.authority {
            self.verified
                .insert(authority.local_identity.clone(), (0, lsn));
        }
    }

    pub fn observe_secondary_removal(&mut self, witness: &SecondaryRemovalWitness) -> Result<()> {
        if self.witnesses.get(&witness.identity) == Some(witness) {
            return Ok(());
        }
        let authority = self
            .authority
            .as_ref()
            .ok_or(RuntimeError::AuthorityNotAdmitted)?;
        let evidence = authority
            .secondary_removal
            .as_ref()
            .ok_or(RuntimeError::ReconfigurationPending)?;
        let intent = &evidence.preparation.intent;
        let stage = if authority.previous_configuration.is_some() {
            SecondaryRemovalStage::PreviousCurrent
        } else {
            SecondaryRemovalStage::CurrentOnly
        };
        if witness.identity == authority.local_identity
            || self.sessions.get(&witness.identity) != Some(&witness.process_session_id)
            || witness.resource_uid != intent.resource_uid
            || !intent
                .current_configuration
                .members
                .iter()
                .any(|m| m.identity == witness.identity && m.role == witness.role)
            || witness.epoch != authority.current_configuration.epoch
            || witness.current_configuration_id != authority.current_configuration.configuration_id
            || witness.previous_configuration_id != authority.fence().previous_configuration_id
            || witness.report_sequence == 0
            || witness.verified_replication_lsn < evidence.preparation.boundary_lsn
            || witness.pending_operation_id.is_some()
            || witness.write_status == kuberic_protocol::types::AccessStatus::Granted
            || witness.retained_operation_id.as_ref()
                != Some(&intent.command_operation_id(stage, &witness.identity))
            || self
                .verified
                .get(&witness.identity)
                .is_some_and(|(seq, _)| *seq >= witness.report_sequence)
            || evidence
                .previous_read_quorum
                .iter()
                .chain(if authority.previous_configuration.is_none() {
                    evidence.reduced_write_quorum.as_slice()
                } else {
                    &[]
                })
                .any(|old| {
                    old.identity == witness.identity
                        && old.process_session_id == witness.process_session_id
                        && old.report_sequence >= witness.report_sequence
                })
        {
            return Err(RuntimeError::AuthorityMismatch(
                "stale or unverified reduced-quorum witness".into(),
            ));
        }
        self.verified.insert(
            witness.identity.clone(),
            (witness.report_sequence, witness.verified_replication_lsn),
        );
        self.witnesses
            .insert(witness.identity.clone(), witness.clone());
        Ok(())
    }

    pub fn register_write(&mut self, lsn: Lsn) -> Result<oneshot::Receiver<Result<Lsn>>> {
        if self.authority.is_none() {
            return Err(RuntimeError::AuthorityNotAdmitted);
        }
        self.highest_lsn = self.highest_lsn.max(lsn);
        let (sender, receiver) = oneshot::channel();
        self.pending.entry(lsn).or_default().push(sender);
        Ok(receiver)
    }

    pub fn record_local_progress(&mut self, lsn: Lsn) -> Result<()> {
        let authority = self
            .authority
            .as_ref()
            .ok_or(RuntimeError::AuthorityNotAdmitted)?;
        self.progress
            .entry(authority.local_identity.clone())
            .and_modify(|progress| *progress = (*progress).max(lsn))
            .or_insert(lsn);
        self.highest_lsn = self.highest_lsn.max(lsn);
        Ok(())
    }

    pub fn record_build_handoff_progress(
        &mut self,
        identity: ReplicaIdentity,
        lsn: Lsn,
    ) -> Result<()> {
        let authority = self
            .authority
            .as_ref()
            .ok_or(RuntimeError::AuthorityNotAdmitted)?;
        if !authority.contains_member(&identity) {
            return Err(RuntimeError::AuthorityMismatch(
                "durable progress belongs to a replica outside authority".to_string(),
            ));
        }
        self.progress
            .entry(identity)
            .and_modify(|progress| *progress = (*progress).max(lsn))
            .or_insert(lsn);
        self.highest_lsn = self.highest_lsn.max(lsn);
        Ok(())
    }

    pub fn acknowledge(&mut self, acknowledgement: &ReplicationAck) -> Result<()> {
        let authority = self
            .authority
            .as_ref()
            .ok_or(RuntimeError::AuthorityNotAdmitted)?;
        authority.validate_acknowledgement(acknowledgement)?;
        self.progress
            .entry(acknowledgement.receiver.clone())
            .and_modify(|progress| {
                *progress = (*progress).max(acknowledgement.applied_lsn);
            })
            .or_insert(acknowledgement.applied_lsn);
        Ok(())
    }

    pub fn acknowledge_in_session(
        &mut self,
        acknowledgement: &ReplicationAck,
        session: &ProcessSessionId,
    ) -> Result<()> {
        if self.sessions.get(&acknowledgement.receiver) != Some(session) {
            return Err(RuntimeError::AuthorityMismatch(
                "acknowledgement belongs to an obsolete peer session".into(),
            ));
        }
        self.acknowledge(acknowledgement)
    }

    pub fn current_configuration_quorum_progress(&self) -> Lsn {
        self.authority.as_ref().map_or(0, |authority| {
            quorum_progress(&authority.current_configuration, &self.progress)
        })
    }

    pub fn committed_lsn(&self) -> Lsn {
        self.committed_lsn
    }

    pub fn highest_lsn(&self) -> Lsn {
        self.highest_lsn
    }

    pub fn catch_up_boundary(&self) -> Option<Lsn> {
        self.catch_up_boundary
    }

    pub fn fail_pending(&mut self) {
        for (_, senders) in std::mem::take(&mut self.pending) {
            for sender in senders {
                let _ = sender.send(Err(RuntimeError::WriteClosed(
                    kuberic_protocol::types::AccessStatus::ReconfigurationPending,
                )));
            }
        }
    }

    pub fn catch_up_complete(&self) -> bool {
        if let Some(authority) = &self.authority
            && let Some(evidence) = &authority.secondary_removal
        {
            let boundary = evidence.preparation.boundary_lsn;
            return self
                .verified
                .get(authority.primary_identity())
                .is_some_and(|(_, lsn)| *lsn >= boundary)
                && authority
                    .current_configuration
                    .members
                    .iter()
                    .filter(|m| {
                        self.verified
                            .get(&m.identity)
                            .is_some_and(|(_, lsn)| *lsn >= boundary)
                    })
                    .count()
                    >= authority.current_configuration.write_quorum as usize;
        }
        let Some(boundary) = self.catch_up_boundary else {
            return true;
        };
        let quorum_progress = self.current_configuration_quorum_progress();
        quorum_progress >= boundary
            && self.must_catch_up.iter().all(|identity| {
                self.progress.get(identity).copied().unwrap_or(0) >= quorum_progress
            })
    }

    pub(crate) fn all_caught_up(&self, lsn: Lsn) -> bool {
        self.authority.as_ref().is_some_and(|authority| {
            authority
                .current_configuration
                .members
                .iter()
                .all(|member| self.progress.get(&member.identity).copied().unwrap_or(0) >= lsn)
        })
    }

    pub fn ready_commit_lsn(&self) -> Option<Lsn> {
        let authority = self.authority.as_ref()?;
        self.pending
            .keys()
            .copied()
            .filter(|lsn| client_commit_ready(authority, &self.progress, *lsn))
            .max()
    }

    pub fn finalize_commit(&mut self, committed_lsn: Lsn) -> Result<()> {
        let ready = self.ready_commit_lsn().ok_or_else(|| {
            RuntimeError::Application("no quorum-ready write can be finalized".to_string())
        })?;
        if committed_lsn > ready {
            return Err(RuntimeError::Application(
                "application commit exceeds quorum-ready progress".to_string(),
            ));
        }
        let completed = self
            .pending
            .keys()
            .copied()
            .take_while(|lsn| *lsn <= committed_lsn)
            .collect::<Vec<_>>();
        for lsn in completed {
            if let Some(senders) = self.pending.remove(&lsn) {
                for sender in senders {
                    let _ = sender.send(Ok(lsn));
                }
            }
        }
        self.committed_lsn = self.committed_lsn.max(committed_lsn);
        Ok(())
    }

    pub fn restore_committed_lsn(&mut self, committed_lsn: Lsn) {
        let completed = self
            .pending
            .keys()
            .copied()
            .take_while(|lsn| *lsn <= committed_lsn)
            .collect::<Vec<_>>();
        for lsn in completed {
            if let Some(senders) = self.pending.remove(&lsn) {
                for sender in senders {
                    let _ = sender.send(Ok(lsn));
                }
            }
        }
        self.highest_lsn = self.highest_lsn.max(committed_lsn);
        self.committed_lsn = self.committed_lsn.max(committed_lsn);
    }

    pub fn reset_progress_after_data_loss(
        &mut self,
        local_identity: ReplicaIdentity,
        current_progress: Lsn,
        committed_lsn: Lsn,
    ) {
        self.fail_pending();
        self.progress.clear();
        self.progress.insert(local_identity, current_progress);
        self.highest_lsn = current_progress;
        self.committed_lsn = committed_lsn;
        self.catch_up_boundary = None;
        self.must_catch_up.clear();
        self.sessions.clear();
        self.verified.clear();
        self.witnesses.clear();
    }
}

fn authority_members(authority: &AdmittedAuthority) -> BTreeSet<ReplicaIdentity> {
    authority
        .current_configuration
        .members
        .iter()
        .chain(
            authority
                .previous_configuration
                .iter()
                .flat_map(|configuration| configuration.members.iter()),
        )
        .map(|member| member.identity.clone())
        .collect()
}

fn derive_must_catch_up(authority: &AdmittedAuthority) -> BTreeSet<ReplicaIdentity> {
    let Some(previous) = authority.previous_configuration.as_ref() else {
        return BTreeSet::new();
    };
    let current_primary = authority
        .current_configuration
        .members
        .iter()
        .find(|member| {
            member.identity.replica_id == authority.current_configuration.primary_id
                && member.role == ReplicaRole::Primary
        })
        .expect("validated configuration has one primary");
    let was_same_primary = previous.members.iter().any(|member| {
        member.identity == current_primary.identity && member.role == ReplicaRole::Primary
    });
    if was_same_primary {
        BTreeSet::new()
    } else {
        BTreeSet::from([current_primary.identity.clone()])
    }
}

fn quorum_progress(
    configuration: &ConfigurationDescriptor,
    progress: &BTreeMap<ReplicaIdentity, Lsn>,
) -> Lsn {
    let mut values = configuration
        .members
        .iter()
        .map(|member| progress.get(&member.identity).copied().unwrap_or(0))
        .collect::<Vec<_>>();
    values.sort_unstable_by(|left, right| right.cmp(left));
    values
        .get(configuration.write_quorum.saturating_sub(1) as usize)
        .copied()
        .unwrap_or(0)
}

fn client_commit_ready(
    authority: &AdmittedAuthority,
    progress: &BTreeMap<ReplicaIdentity, Lsn>,
    lsn: Lsn,
) -> bool {
    has_quorum(&authority.current_configuration, progress, lsn)
        && authority
            .previous_configuration
            .as_ref()
            .is_none_or(|previous| has_quorum(previous, progress, lsn))
}

fn has_quorum(
    configuration: &ConfigurationDescriptor,
    progress: &BTreeMap<ReplicaIdentity, Lsn>,
    lsn: Lsn,
) -> bool {
    configuration
        .members
        .iter()
        .filter(|member| progress.get(&member.identity).copied().unwrap_or(0) >= lsn)
        .count()
        >= configuration.write_quorum as usize
}
