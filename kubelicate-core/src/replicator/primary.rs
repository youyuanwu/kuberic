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

/// A connection to one secondary replica for sending replication operations
/// and receiving ACKs.
pub struct SecondaryConnection {
    /// Send items to this secondary's gRPC stream
    item_tx: mpsc::Sender<ReplicationItem>,
}

/// Primary-side replication sender. Manages connections to all configured
/// secondaries, sends operations, and routes ACKs back to the QuorumTracker.
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
    /// Spawns a background task that streams items and routes ACKs.
    pub async fn add_secondary(
        &mut self,
        replica_id: ReplicaId,
        address: String,
        quorum_tracker: Arc<tokio::sync::Mutex<QuorumTracker>>,
    ) -> crate::Result<()> {
        let channel = Channel::from_shared(address)
            .map_err(|e| crate::KubelicateError::Internal(Box::new(e)))?
            .connect()
            .await
            .map_err(|e| crate::KubelicateError::Internal(Box::new(e)))?;

        let mut client = ReplicatorDataClient::new(channel);

        let (item_tx, item_rx) = mpsc::channel::<ReplicationItem>(256);
        let outbound = ReceiverStream::new(item_rx);

        let response = client
            .replication_stream(outbound)
            .await
            .map_err(|e| crate::KubelicateError::Internal(Box::new(e)))?;

        let mut ack_stream = response.into_inner();
        let rid = replica_id;

        // Spawn ACK reader — routes ACKs to the quorum tracker
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

        self.connections
            .insert(replica_id, SecondaryConnection { item_tx });

        Ok(())
    }

    /// Remove a secondary connection.
    pub fn remove_secondary(&mut self, replica_id: ReplicaId) {
        self.connections.remove(&replica_id);
    }

    /// Send an operation to all connected secondaries.
    pub async fn send_to_all(&self, lsn: Lsn, data: &bytes::Bytes) {
        let item = ReplicationItem {
            epoch_data_loss: self.epoch.data_loss_number,
            epoch_config: self.epoch.configuration_number,
            lsn,
            data: data.to_vec(),
        };

        for (rid, conn) in &self.connections {
            if conn.item_tx.send(item.clone()).await.is_err() {
                warn!(
                    replica_id = rid,
                    lsn, "failed to send to secondary — channel closed"
                );
            }
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

    /// Close all connections (drops senders, which closes gRPC streams).
    pub fn close_all(&mut self) {
        self.connections.clear();
    }
}
