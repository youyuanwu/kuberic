pub mod copy;
pub mod queue;
pub mod quorum;

use std::collections::BTreeSet;

use kuberic_protocol::types::ReplicaIdentity;
use kuberic_wire::{ReplicationAcknowledgement, proto};
use tokio::sync::oneshot;

use crate::application::{DurableApplicationAck, Lsn, Operation};
use crate::authority::AdmittedAuthority;
use crate::replicator::queue::ReplicationQueue;
use crate::{Result, RuntimeError};

use self::quorum::QuorumTracker;

#[derive(Debug)]
pub struct PreparedWrite {
    pub lsn: Lsn,
    pub items: Vec<proto::ReplicationItem>,
    pub completion: oneshot::Receiver<Result<Lsn>>,
}

#[derive(Debug)]
pub struct Replicator {
    local_identity: ReplicaIdentity,
    authority: Option<AdmittedAuthority>,
    next_lsn: Lsn,
    queue: ReplicationQueue,
    quorum: QuorumTracker,
}

impl Replicator {
    pub fn new(local_identity: ReplicaIdentity) -> Self {
        Self {
            local_identity,
            authority: None,
            next_lsn: 0,
            queue: ReplicationQueue::default(),
            quorum: QuorumTracker::default(),
        }
    }

    pub fn configure(&mut self, authority: AdmittedAuthority, local_progress: Lsn) -> Result<()> {
        if authority.local_identity != self.local_identity {
            return Err(RuntimeError::AuthorityMismatch(
                "admitted local identity differs from runtime identity".to_string(),
            ));
        }
        self.next_lsn = self.next_lsn.max(local_progress);
        self.quorum.configure(authority.clone(), local_progress)?;
        self.authority = Some(authority);
        Ok(())
    }

    pub fn reserve_lsn(&mut self) -> Result<Lsn> {
        let authority = self
            .authority
            .as_ref()
            .ok_or(RuntimeError::AuthorityNotAdmitted)?;
        if authority.primary_identity() != &self.local_identity {
            return Err(RuntimeError::NotPrimary);
        }
        self.next_lsn += 1;
        Ok(self.next_lsn)
    }

    pub fn record_local_write(
        &mut self,
        operation: Operation,
        durable_ack: DurableApplicationAck,
    ) -> Result<PreparedWrite> {
        if durable_ack.applied_lsn != operation.lsn
            || durable_ack.committed_lsn < 0
            || durable_ack.committed_lsn > durable_ack.applied_lsn
        {
            return Err(RuntimeError::Application(
                "durable acknowledgement does not match the local operation".to_string(),
            ));
        }
        let authority = self
            .authority
            .as_ref()
            .ok_or(RuntimeError::AuthorityNotAdmitted)?;
        self.queue.push(operation.clone());
        self.quorum.record_local_progress(operation.lsn)?;
        let completion = self.quorum.register_write(operation.lsn)?;
        let targets = authority
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
            .filter(|identity| identity != &self.local_identity)
            .collect::<BTreeSet<_>>();
        let items = targets
            .into_iter()
            .map(|receiver| proto::ReplicationItem {
                protocol_version: kuberic_protocol::PROTOCOL_VERSION,
                sender: Some(self.local_identity.clone().into()),
                epoch: Some(authority.current_configuration.epoch.into()),
                previous_configuration_id: authority
                    .previous_configuration
                    .as_ref()
                    .map_or_else(String::new, |configuration| {
                        configuration.configuration_id.to_string()
                    }),
                current_configuration_id: authority
                    .current_configuration
                    .configuration_id
                    .to_string(),
                lsn: operation.lsn,
                committed_lsn: self.quorum.committed_lsn(),
                data: operation.data.to_vec(),
                receiver: Some(receiver.into()),
            })
            .collect();
        self.queue.truncate_committed(self.quorum.committed_lsn());
        Ok(PreparedWrite {
            lsn: operation.lsn,
            items,
            completion,
        })
    }

    pub fn acknowledge(&mut self, acknowledgement: &ReplicationAcknowledgement) -> Result<()> {
        self.quorum.acknowledge(acknowledgement)
    }

    pub fn record_local_progress(&mut self, lsn: Lsn) -> Result<()> {
        self.next_lsn = self.next_lsn.max(lsn);
        self.quorum.record_local_progress(lsn)
    }

    pub fn committed_lsn(&self) -> Lsn {
        self.quorum.committed_lsn()
    }

    pub fn ready_commit_lsn(&self) -> Option<Lsn> {
        self.quorum.ready_commit_lsn()
    }

    pub fn finalize_commit(&mut self, committed_lsn: Lsn) -> Result<()> {
        self.quorum.finalize_commit(committed_lsn)?;
        self.queue.truncate_committed(committed_lsn);
        Ok(())
    }

    pub fn current_configuration_quorum_progress(&self) -> Lsn {
        self.quorum.current_configuration_quorum_progress()
    }

    pub fn catch_up_boundary(&self) -> Option<Lsn> {
        self.quorum.catch_up_boundary()
    }

    pub fn retained_operations_from(&self, from_lsn: Lsn) -> Vec<Operation> {
        self.queue.operations_from(from_lsn)
    }

    pub fn catch_up_complete(&self) -> bool {
        self.quorum.catch_up_complete()
    }

    pub fn highest_lsn(&self) -> Lsn {
        self.quorum.highest_lsn()
    }

    pub fn fence_client_writes(&mut self) {
        self.quorum.fail_pending();
    }
}
