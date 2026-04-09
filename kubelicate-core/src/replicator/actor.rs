use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use tokio::sync::{Mutex as TokioMutex, mpsc};
use tracing::{info, warn};

use crate::error::KubelicateError;
use crate::events::{ReplicateRequest, ReplicatorControlEvent};
use crate::handles::PartitionState;
use crate::replicator::primary::PrimarySender;
use crate::replicator::queue::ReplicationQueue;
use crate::replicator::quorum::QuorumTracker;
use crate::types::{DataLossAction, Epoch, Lsn, ReplicaId, Role};

/// The WalReplicator actor. Processes control and data events in a single
/// loop with biased select (control has priority). The data path is
/// non-blocking because PrimarySender::send_to_all uses unbounded channels
/// with per-secondary drain tasks (matching SF's async dispatch model).
///
/// Owns a ReplicationQueue that retains ops for replay to new replicas,
/// matching SF's ReplicationQueueManager pattern.
pub struct WalReplicatorActor {
    replica_id: ReplicaId,
}

impl WalReplicatorActor {
    pub fn new(replica_id: ReplicaId) -> Self {
        Self { replica_id }
    }

    #[allow(unused_assignments)]
    pub async fn run(
        self,
        mut control_rx: mpsc::Receiver<ReplicatorControlEvent>,
        mut data_rx: mpsc::Receiver<ReplicateRequest>,
        state: Arc<PartitionState>,
    ) {
        let mut role = Role::None;
        let mut epoch = Epoch::default();
        let mut next_lsn: Lsn = 1;
        let quorum_tracker = Arc::new(TokioMutex::new(QuorumTracker::new()));
        let mut primary_sender: Option<PrimarySender> = None;
        let mut replication_queue = ReplicationQueue::new();

        loop {
            tokio::select! {
                biased;

                event = control_rx.recv() => {
                    let Some(event) = event else { break };
                    match event {
                        ReplicatorControlEvent::Open { reply, .. } => {
                            info!(replica_id = self.replica_id, "replicator opened");
                            let _ = reply.send(Ok(()));
                        }
                        ReplicatorControlEvent::Close { reply } => {
                            info!(replica_id = self.replica_id, "replicator closing");
                            quorum_tracker.lock().await.fail_all(KubelicateError::Closed);
                            if let Some(mut sender) = primary_sender.take() {
                                sender.close_all();
                            }
                            replication_queue.clear();
                            let _ = reply.send(Ok(()));
                            break;
                        }
                        ReplicatorControlEvent::Abort => {
                            quorum_tracker.lock().await.fail_all(KubelicateError::Closed);
                            if let Some(mut sender) = primary_sender.take() {
                                sender.close_all();
                            }
                            replication_queue.clear();
                            break;
                        }
                        ReplicatorControlEvent::ChangeRole {
                            epoch: new_epoch,
                            role: new_role,
                            reply,
                        } => {
                            info!(
                                replica_id = self.replica_id,
                                ?new_role,
                                ?new_epoch,
                                "replicator changing role"
                            );

                            if role == Role::Primary && new_role != Role::Primary {
                                quorum_tracker.lock().await.fail_all(KubelicateError::NotPrimary);
                                if let Some(mut sender) = primary_sender.take() {
                                    sender.close_all();
                                }
                                replication_queue.clear();
                            }

                            epoch = new_epoch;
                            role = new_role;

                            if role == Role::Primary {
                                primary_sender = Some(PrimarySender::new(self.replica_id, epoch));
                            }

                            let _ = reply.send(Ok(()));
                        }
                        ReplicatorControlEvent::UpdateEpoch {
                            epoch: new_epoch,
                            reply,
                        } => {
                            info!(
                                replica_id = self.replica_id,
                                ?new_epoch,
                                "updating epoch"
                            );
                            epoch = new_epoch;
                            let _ = reply.send(Ok(()));
                        }
                        ReplicatorControlEvent::UpdateCatchUpConfiguration {
                            current,
                            previous,
                            reply,
                        } => {
                            let mut cc_members: HashSet<ReplicaId> =
                                current.members.iter().map(|r| r.id).collect();
                            cc_members.insert(self.replica_id);
                            let mut pc_members: HashSet<ReplicaId> =
                                previous.members.iter().map(|r| r.id).collect();
                            if !pc_members.is_empty() {
                                pc_members.insert(self.replica_id);
                            }

                            let must_catch_up: HashSet<ReplicaId> = current
                                .members
                                .iter()
                                .filter(|r| r.must_catch_up)
                                .map(|r| r.id)
                                .collect();

                            let member_progress: HashMap<ReplicaId, Lsn> = current
                                .members
                                .iter()
                                .map(|r| (r.id, r.current_progress))
                                .collect();

                            quorum_tracker.lock().await.set_catch_up_configuration(
                                cc_members,
                                current.write_quorum,
                                pc_members,
                                previous.write_quorum,
                                must_catch_up,
                                member_progress,
                            );

                            // Connect new secondaries and replay pending ops
                            if let Some(sender) = &mut primary_sender {
                                for member in &current.members {
                                    if member.id != self.replica_id
                                        && !sender.has_connection(&member.id)
                                    {
                                        if let Err(e) = sender
                                            .add_secondary(
                                                member.id,
                                                member.replicator_address.clone(),
                                                quorum_tracker.clone(),
                                            )
                                            .await
                                        {
                                            warn!(
                                                replica_id = member.id,
                                                error = %e,
                                                "failed to connect to secondary"
                                            );
                                            continue;
                                        }

                                        // Replay all ops from replication queue.
                                        // Some may duplicate the copy stream — that's
                                        // OK, apply_op is idempotent (same key = overwrite).
                                        let pending = replication_queue.ops_from(1);
                                        if !pending.is_empty() {
                                            info!(
                                                replica_id = member.id,
                                                count = pending.len(),
                                                "replaying ops from replication queue"
                                            );
                                            for (lsn, data) in &pending {
                                                sender.send_to_one(member.id, *lsn, data);
                                            }
                                        }
                                    }
                                }
                            }

                            let _ = reply.send(Ok(()));
                        }
                        ReplicatorControlEvent::UpdateCurrentConfiguration {
                            current,
                            reply,
                        } => {
                            let mut cc_members: HashSet<ReplicaId> =
                                current.members.iter().map(|r| r.id).collect();
                            cc_members.insert(self.replica_id);

                            quorum_tracker.lock().await.set_current_configuration(
                                cc_members.clone(),
                                current.write_quorum,
                            );

                            if let Some(sender) = &mut primary_sender {
                                let to_remove: Vec<ReplicaId> = sender
                                    .connected_ids()
                                    .into_iter()
                                    .filter(|id| !cc_members.contains(id))
                                    .collect();
                                for id in to_remove {
                                    sender.remove_secondary(id);
                                }
                            }

                            let _ = reply.send(Ok(()));

                            // GC replication queue — config is finalized,
                            // all replicas are caught up. Safe to remove
                            // ops up to committed_lsn.
                            let committed = state.committed_lsn();
                            replication_queue.gc(committed);
                        }
                        ReplicatorControlEvent::WaitForCatchUpQuorum { mode, reply } => {
                            quorum_tracker.lock().await.wait_for_catch_up(mode, reply);
                        }
                        ReplicatorControlEvent::BuildReplica { replica, reply } => {
                            // No-op with replication queue design — pending ops
                            // are replayed at add_secondary time from the queue.
                            info!(
                                replica_id = replica.id,
                                queue_len = replication_queue.len(),
                                "BuildReplica: queue has ops for replay at connect time"
                            );
                            let _ = reply.send(Ok(()));
                        }
                        ReplicatorControlEvent::RemoveReplica { replica_id, reply } => {
                            if let Some(sender) = &mut primary_sender {
                                sender.remove_secondary(replica_id);
                            }
                            let _ = reply.send(Ok(()));
                        }
                        ReplicatorControlEvent::OnDataLoss { reply } => {
                            let _ = reply.send(Ok(DataLossAction::None));
                        }
                    }
                }

                req = data_rx.recv(), if role == Role::Primary => {
                    let Some(req) = req else { break };
                    let lsn = next_lsn;
                    next_lsn += 1;

                    // Store in replication queue for replay to new replicas
                    replication_queue.push(lsn, req.data.clone());

                    // Register with quorum tracker (primary's own ACK counted)
                    quorum_tracker.lock().await.register(lsn, self.replica_id, req.reply);

                    // Non-blocking: send_to_all uses unbounded channels
                    if let Some(sender) = &mut primary_sender {
                        sender.send_to_all(lsn, &req.data);
                    }

                    // Update progress and committed LSN
                    state.set_current_progress(lsn);
                    let committed = quorum_tracker.lock().await.committed_lsn();
                    state.set_committed_lsn(committed);
                }

                else => break,
            }
        }
    }
}
