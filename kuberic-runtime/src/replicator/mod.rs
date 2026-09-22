pub mod copy;
pub mod queue;
pub mod quorum;

use std::collections::BTreeSet;

use bytes::Bytes;
use kuberic_protocol::types::{OperationId, ReplicaIdentity};
use kuberic_wire::{ReplicationAcknowledgement, proto};
use tokio::sync::oneshot;

use crate::application::{ClientWrite, Lsn, Operation};
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
    pending_local_write: Option<PendingLocalWrite>,
    queue: ReplicationQueue,
    quorum: QuorumTracker,
}

#[derive(Debug, Clone)]
struct PendingLocalWrite {
    operation_id: OperationId,
    lsn: Lsn,
    data: Bytes,
}

impl Replicator {
    pub fn new(local_identity: ReplicaIdentity) -> Self {
        Self {
            local_identity,
            authority: None,
            next_lsn: 0,
            pending_local_write: None,
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
        if self.authority.as_ref() != Some(&authority) {
            self.pending_local_write = None;
        }
        self.next_lsn = self.next_lsn.max(local_progress);
        self.quorum.configure(authority.clone(), local_progress)?;
        self.authority = Some(authority);
        Ok(())
    }

    pub fn reserve_write(&mut self, write: &ClientWrite) -> Result<Lsn> {
        let authority = self
            .authority
            .as_ref()
            .ok_or(RuntimeError::AuthorityNotAdmitted)?;
        if authority.primary_identity() != &self.local_identity {
            return Err(RuntimeError::NotPrimary);
        }
        if let Some(pending) = &self.pending_local_write {
            if pending.operation_id == write.operation_id && pending.data == write.data {
                return Ok(pending.lsn);
            }
            return Err(RuntimeError::LocalWritePending(
                pending.operation_id.to_string(),
            ));
        }
        let lsn = self.next_lsn + 1;
        self.pending_local_write = Some(PendingLocalWrite {
            operation_id: write.operation_id.clone(),
            lsn,
            data: write.data.clone(),
        });
        Ok(lsn)
    }

    pub fn restore_write_reservation(&mut self, write: &ClientWrite, lsn: Lsn) -> Result<()> {
        if let Some(pending) = &self.pending_local_write {
            if pending.operation_id == write.operation_id
                && pending.lsn == lsn
                && pending.data == write.data
            {
                return Ok(());
            }
            return Err(RuntimeError::LocalWritePending(
                pending.operation_id.to_string(),
            ));
        }
        self.pending_local_write = Some(PendingLocalWrite {
            operation_id: write.operation_id.clone(),
            lsn,
            data: write.data.clone(),
        });
        self.next_lsn = self.next_lsn.max(lsn - 1);
        Ok(())
    }

    pub fn ensure_local_write_registered(
        &mut self,
        operation: &Operation,
    ) -> Result<PreparedWrite> {
        if let Some(pending) = self.pending_local_write.as_ref()
            && (pending.lsn != operation.lsn || pending.data != operation.data)
        {
            return Err(RuntimeError::Application(
                "local write differs from its reserved operation".to_string(),
            ));
        }
        self.queue.push(operation.clone());
        self.quorum.record_local_progress(operation.lsn)?;
        let completion = if operation.lsn <= self.quorum.committed_lsn() {
            let (sender, receiver) = oneshot::channel();
            let _ = sender.send(Ok(operation.lsn));
            receiver
        } else {
            self.quorum.register_write(operation.lsn)?
        };
        let items = self.replication_items(operation)?;
        self.queue.truncate_committed(self.quorum.committed_lsn());
        self.next_lsn = self.next_lsn.max(operation.lsn);
        self.pending_local_write = None;
        Ok(PreparedWrite {
            lsn: operation.lsn,
            items,
            completion,
        })
    }

    fn replication_items(&self, operation: &Operation) -> Result<Vec<proto::ReplicationItem>> {
        let authority = self
            .authority
            .as_ref()
            .ok_or(RuntimeError::AuthorityNotAdmitted)?;
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
        Ok(targets
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
            .collect())
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

    pub fn restore_committed_write(&mut self, operation: &Operation) -> Result<()> {
        if let Some(pending) = self.pending_local_write.as_ref()
            && (pending.lsn != operation.lsn || pending.data != operation.data)
        {
            return Err(RuntimeError::Application(
                "committed write conflicts with the active reservation".to_string(),
            ));
        }
        self.pending_local_write = None;
        self.next_lsn = self.next_lsn.max(operation.lsn);
        self.quorum.record_local_progress(operation.lsn)?;
        self.quorum.restore_committed_lsn(operation.lsn);
        self.queue.truncate_committed(operation.lsn);
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
