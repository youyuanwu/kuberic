use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::mpsc;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::ReceiverStream;
use tonic::transport::Channel;
use tracing::{debug, warn};

use crate::proto::ReplicationItem;
use crate::proto::replicator_data_client::ReplicatorDataClient;
use crate::replicator::quorum::QuorumTracker;
use crate::types::{Epoch, Lsn, ReplicaId};

/// A connection to one secondary replica. Uses an unbounded channel so
/// send_to_all never blocks the actor. A background drain task bridges
/// the unbounded channel to the bounded gRPC stream.
struct SecondaryConnection {
    /// Unbounded sender — send_to_all writes here, never blocks
    item_tx: mpsc::UnboundedSender<ReplicationItem>,
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
    #[allow(dead_code)]
    primary_id: ReplicaId,
    epoch: Epoch,
}

impl PrimarySender {
    pub fn new(primary_id: ReplicaId, epoch: Epoch) -> Self {
        Self {
            connections: HashMap::new(),
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
        address: String,
        quorum_tracker: Arc<tokio::sync::Mutex<QuorumTracker>>,
    ) -> crate::Result<()> {
        if self.connections.contains_key(&replica_id) {
            return Ok(()); // already connected
        }

        let channel = Channel::from_shared(address)
            .map_err(|e| crate::KubelicateError::Internal(Box::new(e)))?
            .connect()
            .await
            .map_err(|e| crate::KubelicateError::Internal(Box::new(e)))?;

        let mut client = ReplicatorDataClient::new(channel);

        // Bounded channel for the gRPC stream (backpressure at transport level)
        let (grpc_tx, grpc_rx) = mpsc::channel::<ReplicationItem>(256);
        let outbound = ReceiverStream::new(grpc_rx);

        let response = client
            .replication_stream(outbound)
            .await
            .map_err(|e| crate::KubelicateError::Internal(Box::new(e)))?;

        let mut ack_stream = response.into_inner();
        let rid = replica_id;

        // Spawn ACK reader
        tokio::spawn(async move {
            while let Some(result) = ack_stream.next().await {
                match result {
                    Ok(ack) => {
                        debug!(replica_id = rid, lsn = ack.lsn, "received ACK");
                        quorum_tracker.lock().await.ack(ack.lsn, rid);
                    }
                    Err(e) => {
                        warn!(replica_id = rid, error = %e, "ACK stream error");
                        break;
                    }
                }
            }
        });

        // Unbounded channel: send_to_all writes here (never blocks)
        let (unbounded_tx, mut unbounded_rx) = mpsc::unbounded_channel::<ReplicationItem>();

        // Spawn drain task: bridges unbounded → bounded gRPC stream.
        // This task may block on grpc_tx.send() if the secondary is slow,
        // but that only blocks THIS task, not the actor.
        tokio::spawn(async move {
            while let Some(item) = unbounded_rx.recv().await {
                if grpc_tx.send(item).await.is_err() {
                    warn!(replica_id = rid, "gRPC stream closed, drain task exiting");
                    break;
                }
            }
        });

        self.connections.insert(
            replica_id,
            SecondaryConnection {
                item_tx: unbounded_tx,
            },
        );

        Ok(())
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
    pub fn remove_secondary(&mut self, replica_id: ReplicaId) {
        self.connections.remove(&replica_id);
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
            self.connections.remove(&rid);
        }
    }

    /// Number of connected secondaries.
    pub fn connection_count(&self) -> usize {
        self.connections.len()
    }

    /// Check if a secondary is connected.
    pub fn has_connection(&self, replica_id: &ReplicaId) -> bool {
        self.connections.contains_key(replica_id)
    }

    /// Get all connected replica IDs.
    pub fn connected_ids(&self) -> Vec<ReplicaId> {
        self.connections.keys().cloned().collect()
    }

    /// Close all connections.
    pub fn close_all(&mut self) {
        self.connections.clear();
    }
}
