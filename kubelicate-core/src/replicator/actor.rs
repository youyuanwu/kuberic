use std::collections::HashSet;
use std::sync::Arc;

use tokio::sync::{Mutex as TokioMutex, mpsc};
use tracing::{info, warn};

use crate::error::KubelicateError;
use crate::events::{ReplicateRequest, ReplicatorControlEvent};
use crate::handles::PartitionState;
use crate::replicator::primary::PrimarySender;
use crate::replicator::quorum::QuorumTracker;
use crate::types::{DataLossAction, Epoch, Lsn, ReplicaId, Role};

/// The WalReplicator actor. Processes control and data events, manages
/// gRPC connections to secondaries, and tracks quorum acknowledgments.
///
/// "Wal" is a misnomer for the MVP — there is no WAL persistence. The name
/// is kept for forward compatibility; persistence will be added later.
pub struct WalReplicatorActor {
    replica_id: ReplicaId,
}

impl WalReplicatorActor {
    pub fn new(replica_id: ReplicaId) -> Self {
        Self { replica_id }
    }

    pub async fn run(
        self,
        mut control_rx: mpsc::Receiver<ReplicatorControlEvent>,
        mut data_rx: mpsc::Receiver<ReplicateRequest>,
        state: Arc<PartitionState>,
    ) {
        let mut role = Role::None;
        #[allow(unused_assignments)]
        let mut epoch = Epoch::default();
        let mut next_lsn: Lsn = 1;
        let quorum_tracker = Arc::new(TokioMutex::new(QuorumTracker::new()));
        let mut primary_sender: Option<PrimarySender> = None;

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
                            // Fail pending operations
                            quorum_tracker.lock().await.fail_all(KubelicateError::Closed);
                            if let Some(mut sender) = primary_sender.take() {
                                sender.close_all();
                            }
                            let _ = reply.send(Ok(()));
                            break;
                        }
                        ReplicatorControlEvent::Abort => {
                            quorum_tracker.lock().await.fail_all(KubelicateError::Closed);
                            if let Some(mut sender) = primary_sender.take() {
                                sender.close_all();
                            }
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

                            // Teardown old role
                            if role == Role::Primary && new_role != Role::Primary {
                                quorum_tracker.lock().await.fail_all(KubelicateError::NotPrimary);
                                if let Some(mut sender) = primary_sender.take() {
                                    sender.close_all();
                                }
                            }

                            epoch = new_epoch;
                            role = new_role;

                            // Setup new role
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
                            let _ = &new_epoch; // used conceptually; actual value tracked for future use
                            epoch = new_epoch;
                            let _ = &epoch;
                            // Truncation: secondaries should truncate uncommitted ops.
                            // For the in-memory MVP, the secondary receiver handles this
                            // via SecondaryState::update_epoch().
                            let _ = reply.send(Ok(()));
                        }
                        ReplicatorControlEvent::UpdateCatchUpConfiguration {
                            current,
                            previous,
                            reply,
                        } => {
                            let mut cc_members: HashSet<ReplicaId> =
                                current.members.iter().map(|r| r.id).collect();
                            cc_members.insert(self.replica_id); // Primary counts toward quorum
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

                            quorum_tracker.lock().await.set_catch_up_configuration(
                                cc_members,
                                current.write_quorum,
                                pc_members,
                                previous.write_quorum,
                                must_catch_up,
                            );

                            // Connect to new secondaries
                            if let Some(sender) = &mut primary_sender {
                                for member in &current.members {
                                    if member.id != self.replica_id
                                        && !sender.has_connection(&member.id)
                                        && let Err(e) = sender
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
                            cc_members.insert(self.replica_id); // Primary counts toward quorum

                            quorum_tracker.lock().await.set_current_configuration(
                                cc_members.clone(),
                                current.write_quorum,
                            );

                            // Remove connections not in current config
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
                        }
                        ReplicatorControlEvent::WaitForCatchUpQuorum { mode, reply } => {
                            quorum_tracker.lock().await.wait_for_catch_up(mode, reply);
                        }
                        ReplicatorControlEvent::BuildReplica { reply, .. } => {
                            // MVP: no copy stream, just acknowledge
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

                    // Register with quorum tracker (primary's own ACK counted)
                    quorum_tracker.lock().await.register(lsn, self.replica_id, req.reply);

                    // Send to all connected secondaries
                    if let Some(sender) = &primary_sender {
                        sender.send_to_all(lsn, &req.data).await;
                    }

                    // Update progress
                    state.set_current_progress(lsn);
                }

                else => break,
            }
        }
    }
}
