use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::mpsc;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::ReceiverStream;
use tonic::transport::Channel;
use tracing::{debug, info, warn};

use crate::proto::ReplicationItem;
use crate::proto::replicator_data_client::ReplicatorDataClient;
use crate::replicator::quorum::QuorumTracker;
use crate::types::{CancellationToken, Epoch, Lsn, ReplicaId, ReplicaInstanceId};

/// A connection to one secondary replica. Uses an unbounded channel so
/// send_to_all never blocks the actor. A background drain task bridges
/// the unbounded channel to the bounded gRPC stream.
struct SecondaryConnection {
    instance_id: ReplicaInstanceId,
    /// Unbounded sender — send_to_all writes here, never blocks
    item_tx: mpsc::UnboundedSender<ReplicationItem>,
    cancellation: CancellationToken,
}

/// Primary-side replication sender. Manages connections to all configured
/// secondaries, sends operations, and routes ACKs back to the QuorumTracker.
///
/// **Non-blocking design (matching SF):** `send_to_all` enqueues ops into
/// per-secondary unbounded channels and returns immediately. Each secondary
/// has a background drain task that reads from the unbounded channel and
/// writes to the gRPC stream. A slow secondary's drain task blocks
/// independently without affecting the actor or other secondaries.
///
/// Pending ops for new replicas are replayed from the ReplicationQueue
/// at `add_secondary` time — no build buffers needed.
pub struct PrimarySender {
    connections: HashMap<ReplicaId, SecondaryConnection>,
    active_instances: Arc<std::sync::RwLock<HashMap<ReplicaId, ReplicaInstanceId>>>,
    #[allow(dead_code)]
    primary_id: ReplicaId,
    epoch: Epoch,
}

impl PrimarySender {
    pub fn new(primary_id: ReplicaId, epoch: Epoch) -> Self {
        Self {
            connections: HashMap::new(),
            active_instances: Arc::new(std::sync::RwLock::new(HashMap::new())),
            primary_id,
            epoch,
        }
    }

    pub fn set_epoch(&mut self, epoch: Epoch) {
        self.epoch = epoch;
    }

    /// Connect to a secondary's replication gRPC endpoint.
    /// If ops were buffered during build, they are replayed first.
    ///
    /// Spawns two background tasks per secondary:
    /// 1. **Drain task**: reads from unbounded channel, writes to bounded
    ///    gRPC stream. May block on slow secondary — only blocks this task.
    /// 2. **ACK reader**: reads ACKs from gRPC response stream, routes to
    ///    QuorumTracker.
    pub async fn add_secondary(
        &mut self,
        replica_id: ReplicaId,
        instance_id: ReplicaInstanceId,
        address: String,
        quorum_tracker: Arc<tokio::sync::Mutex<QuorumTracker>>,
        partition_state: Arc<crate::handles::PartitionState>,
    ) -> crate::Result<()> {
        if self.has_connection(&replica_id, &instance_id) {
            return Ok(()); // already connected
        }

        let channel = Channel::from_shared(address)
            .map_err(|e| crate::KubericError::Internal(Box::new(e)))?
            .connect()
            .await
            .map_err(|e| crate::KubericError::Internal(Box::new(e)))?;

        let mut client = ReplicatorDataClient::new(channel);

        // Bounded channel for the gRPC stream (backpressure at transport level)
        let (grpc_tx, grpc_rx) = mpsc::channel::<ReplicationItem>(256);
        let outbound = ReceiverStream::new(grpc_rx);

        let response = client
            .replication_stream(outbound)
            .await
            .map_err(|e| crate::KubericError::Internal(Box::new(e)))?;

        let mut ack_stream = response.into_inner();
        let rid = replica_id;

        // Unbounded channel: send_to_all writes here (never blocks)
        let (unbounded_tx, mut unbounded_rx) = mpsc::unbounded_channel::<ReplicationItem>();
        let cancellation = CancellationToken::new();
        self.install_connection(
            replica_id,
            SecondaryConnection {
                instance_id: instance_id.clone(),
                item_tx: unbounded_tx,
                cancellation: cancellation.clone(),
            },
        );

        // Spawn drain task: bridges unbounded → bounded gRPC stream.
        // This task may block on grpc_tx.send() if the secondary is slow,
        // but that only blocks THIS task, not the actor.
        let drain_cancellation = cancellation.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    biased;
                    _ = drain_cancellation.cancelled() => break,
                    item = unbounded_rx.recv() => {
                        let Some(item) = item else {
                            break;
                        };
                        let result = tokio::select! {
                            biased;
                            _ = drain_cancellation.cancelled() => break,
                            result = grpc_tx.send(item) => result,
                        };
                        if result.is_err() {
                            warn!(replica_id = rid, "gRPC stream closed, drain task exiting");
                            break;
                        }
                    }
                }
            }
        });

        // Spawn ACK reader only after this incarnation is installed. Cancelling
        // a replaced connection is prioritized over any buffered stale ACK.
        let ps = partition_state;
        let active_instances = self.active_instances.clone();
        tokio::spawn(async move {
            loop {
                let result = tokio::select! {
                    biased;
                    _ = cancellation.cancelled() => break,
                    result = ack_stream.next() => result,
                };
                let Some(result) = result else {
                    break;
                };
                match result {
                    Ok(ack) => {
                        debug!(
                            replica_id = rid,
                            instance_id = %instance_id,
                            lsn = ack.lsn,
                            "received ACK"
                        );
                        let mut tracker = quorum_tracker.lock().await;
                        let active = active_instances.read().unwrap();
                        if active.get(&rid) != Some(&instance_id) {
                            break;
                        }
                        tracker.ack(ack.lsn, rid);
                        ps.advance_committed_lsn(tracker.committed_lsn());
                    }
                    Err(e) => {
                        warn!(
                            replica_id = rid,
                            instance_id = %instance_id,
                            error = %e,
                            "ACK stream error"
                        );
                        break;
                    }
                }
            }
        });

        Ok(())
    }

    fn install_connection(&mut self, replica_id: ReplicaId, connection: SecondaryConnection) {
        let new_instance_id = connection.instance_id.clone();
        self.active_instances
            .write()
            .unwrap()
            .insert(replica_id, new_instance_id.clone());
        if let Some(old) = self.connections.insert(replica_id, connection) {
            info!(
                replica_id,
                old_instance_id = %old.instance_id,
                new_instance_id = %new_instance_id,
                "replacing secondary incarnation"
            );
            old.cancellation.cancel();
        }
    }

    /// Send a single item to a specific secondary (for replay from queue).
    pub fn send_to_one(
        &self,
        replica_id: ReplicaId,
        lsn: Lsn,
        data: &bytes::Bytes,
        committed_lsn: Lsn,
    ) {
        let item = ReplicationItem {
            epoch_data_loss: self.epoch.data_loss_number,
            epoch_config: self.epoch.configuration_number,
            lsn,
            data: data.to_vec(),
            committed_lsn,
        };
        if let Some(conn) = self.connections.get(&replica_id)
            && conn.item_tx.send(item).is_err()
        {
            warn!(replica_id, lsn, "send_to_one: channel closed");
        }
    }

    /// Remove a secondary connection.
    pub fn remove_secondary(
        &mut self,
        replica_id: ReplicaId,
        instance_id: &ReplicaInstanceId,
    ) -> bool {
        let mut active_instances = self.active_instances.write().unwrap();
        if active_instances.get(&replica_id) != Some(instance_id) {
            warn!(
                replica_id,
                instance_id = %instance_id,
                "ignoring removal for stale secondary incarnation"
            );
            return false;
        }
        active_instances.remove(&replica_id);
        if let Some(connection) = self.connections.remove(&replica_id) {
            connection.cancellation.cancel();
        }
        true
    }

    /// Remove whichever incarnation currently owns this logical replica ID.
    pub fn remove_secondary_by_id(&mut self, replica_id: ReplicaId) {
        self.active_instances.write().unwrap().remove(&replica_id);
        if let Some(connection) = self.connections.remove(&replica_id) {
            connection.cancellation.cancel();
        }
    }

    /// Send an operation to all connected secondaries. Non-blocking —
    /// uses unbounded channels. Matches SF's fire-and-forget dispatch.
    pub fn send_to_all(&mut self, lsn: Lsn, data: &bytes::Bytes, committed_lsn: Lsn) {
        let item = ReplicationItem {
            epoch_data_loss: self.epoch.data_loss_number,
            epoch_config: self.epoch.configuration_number,
            lsn,
            data: data.to_vec(),
            committed_lsn,
        };

        let mut dead = Vec::new();
        for (&rid, conn) in &self.connections {
            if conn.item_tx.send(item.clone()).is_err() {
                warn!(
                    replica_id = rid,
                    lsn, "secondary channel closed — removing connection"
                );
                dead.push(rid);
            }
        }
        for rid in dead {
            self.remove_secondary_by_id(rid);
        }
    }

    /// Number of connected secondaries.
    pub fn connection_count(&self) -> usize {
        self.connections.len()
    }

    /// Check if a secondary is connected.
    pub fn has_connection(&self, replica_id: &ReplicaId, instance_id: &ReplicaInstanceId) -> bool {
        self.connections
            .get(replica_id)
            .is_some_and(|connection| connection.instance_id == *instance_id)
    }

    /// Get all connected replica IDs.
    pub fn connected_ids(&self) -> Vec<ReplicaId> {
        self.connections.keys().cloned().collect()
    }

    /// Close all connections.
    pub fn close_all(&mut self) {
        self.active_instances.write().unwrap().clear();
        for connection in self.connections.drain().map(|(_, connection)| connection) {
            connection.cancellation.cancel();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_connection(
        instance_id: &str,
    ) -> (
        SecondaryConnection,
        CancellationToken,
        mpsc::UnboundedReceiver<ReplicationItem>,
    ) {
        let (item_tx, item_rx) = mpsc::unbounded_channel();
        let cancellation = CancellationToken::new();
        (
            SecondaryConnection {
                instance_id: ReplicaInstanceId::new(instance_id),
                item_tx,
                cancellation: cancellation.clone(),
            },
            cancellation,
            item_rx,
        )
    }

    #[test]
    fn installing_new_incarnation_retires_old_connection() {
        let mut sender = PrimarySender::new(1, Epoch::default());
        let (old, old_cancellation, mut old_items) = test_connection("old");
        sender.install_connection(2, old);

        let (new, new_cancellation, mut new_items) = test_connection("new");
        sender.install_connection(2, new);

        assert!(old_cancellation.is_cancelled());
        assert!(!new_cancellation.is_cancelled());
        assert!(sender.has_connection(&2, &ReplicaInstanceId::new("new")));
        assert!(!sender.has_connection(&2, &ReplicaInstanceId::new("old")));
        assert_eq!(
            sender.active_instances.read().unwrap().get(&2),
            Some(&ReplicaInstanceId::new("new"))
        );

        sender.send_to_one(2, 7, &bytes::Bytes::from_static(b"new"), 6);
        assert_eq!(new_items.try_recv().unwrap().lsn, 7);
        assert!(old_items.try_recv().is_err());
    }

    #[test]
    fn stale_incarnation_removal_preserves_current_connection() {
        let mut sender = PrimarySender::new(1, Epoch::default());
        let (current, current_cancellation, _items) = test_connection("current");
        sender.install_connection(2, current);

        assert!(!sender.remove_secondary(2, &ReplicaInstanceId::new("old")));
        assert!(sender.has_connection(&2, &ReplicaInstanceId::new("current")));
        assert!(!current_cancellation.is_cancelled());

        assert!(sender.remove_secondary(2, &ReplicaInstanceId::new("current")));
        assert!(current_cancellation.is_cancelled());
    }
}
