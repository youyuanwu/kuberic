use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::mpsc;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status, Streaming};
use tracing::{debug, warn};

use crate::proto::replicator_data_server::ReplicatorData;
use crate::proto::{ReplicationAck, ReplicationItem};
use crate::types::{Epoch, Lsn};

/// Secondary-side replication receiver. Runs as a gRPC server.
///
/// Validates epoch on incoming operations, stores data in-memory,
/// and sends ACKs back to the primary. On epoch change, truncates
/// uncommitted operations.
pub struct SecondaryReceiver {
    state: Arc<SecondaryState>,
}

pub struct SecondaryState {
    current_epoch: Mutex<Epoch>,
    /// In-memory operation log: LSN → data
    log: Mutex<HashMap<Lsn, bytes::Bytes>>,
    /// Highest LSN received from primary
    received_lsn: AtomicI64,
    /// Highest committed LSN (set by primary via configuration)
    committed_lsn: AtomicI64,
}

impl SecondaryState {
    pub fn new() -> Self {
        Self {
            current_epoch: Mutex::new(Epoch::default()),
            log: Mutex::new(HashMap::new()),
            received_lsn: AtomicI64::new(0),
            committed_lsn: AtomicI64::new(0),
        }
    }

    pub fn received_lsn(&self) -> Lsn {
        self.received_lsn.load(Ordering::Acquire)
    }

    pub fn committed_lsn(&self) -> Lsn {
        self.committed_lsn.load(Ordering::Acquire)
    }

    pub fn set_committed_lsn(&self, lsn: Lsn) {
        self.committed_lsn.store(lsn, Ordering::Release);
    }

    pub fn update_epoch(&self, new_epoch: Epoch) {
        let mut epoch = self.current_epoch.lock().unwrap();
        *epoch = new_epoch;

        // Truncate uncommitted operations
        let committed = self.committed_lsn.load(Ordering::Acquire);
        let mut log = self.log.lock().unwrap();
        log.retain(|lsn, _| *lsn <= committed);

        let new_received = committed.max(self.received_lsn.load(Ordering::Acquire).min(committed));
        self.received_lsn.store(new_received, Ordering::Release);
    }

    pub fn log_len(&self) -> usize {
        self.log.lock().unwrap().len()
    }

    pub fn get(&self, lsn: Lsn) -> Option<bytes::Bytes> {
        self.log.lock().unwrap().get(&lsn).cloned()
    }

    fn accept_item(&self, item: &ReplicationItem) -> Result<(), Status> {
        let epoch = self.current_epoch.lock().unwrap();
        let item_epoch = Epoch::new(item.epoch_data_loss, item.epoch_config);

        if item_epoch < *epoch {
            return Err(Status::failed_precondition(format!(
                "stale epoch: got {:?}, current {:?}",
                item_epoch, *epoch
            )));
        }

        drop(epoch); // release lock before log mutation

        let mut log = self.log.lock().unwrap();
        log.insert(item.lsn, bytes::Bytes::copy_from_slice(&item.data));

        let prev = self.received_lsn.load(Ordering::Acquire);
        if item.lsn > prev {
            self.received_lsn.store(item.lsn, Ordering::Release);
        }

        Ok(())
    }
}

impl Default for SecondaryState {
    fn default() -> Self {
        Self::new()
    }
}

impl SecondaryReceiver {
    pub fn new(state: Arc<SecondaryState>) -> Self {
        Self { state }
    }
}

#[tonic::async_trait]
impl ReplicatorData for SecondaryReceiver {
    type ReplicationStreamStream = ReceiverStream<Result<ReplicationAck, Status>>;

    async fn replication_stream(
        &self,
        request: Request<Streaming<ReplicationItem>>,
    ) -> Result<Response<Self::ReplicationStreamStream>, Status> {
        let mut inbound = request.into_inner();
        let state = self.state.clone();
        let (ack_tx, ack_rx) = mpsc::channel(256);

        tokio::spawn(async move {
            while let Some(result) = inbound.next().await {
                match result {
                    Ok(item) => {
                        let lsn = item.lsn;
                        match state.accept_item(&item) {
                            Ok(()) => {
                                debug!(lsn, "accepted replication item");
                                if ack_tx.send(Ok(ReplicationAck { lsn })).await.is_err() {
                                    break;
                                }
                            }
                            Err(status) => {
                                warn!(lsn, error = %status.message(), "rejected replication item");
                                // Don't ACK stale-epoch items — primary will detect the gap
                                break;
                            }
                        }
                    }
                    Err(e) => {
                        warn!(error = %e, "replication stream error");
                        break;
                    }
                }
            }
        });

        Ok(Response::new(ReceiverStream::new(ack_rx)))
    }
}
