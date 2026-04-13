use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::mpsc;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status, Streaming};
use tracing::{debug, info, warn};

use crate::events::StateProviderEvent;
use crate::proto::replicator_data_server::ReplicatorData;
use crate::proto::{
    CopyItem, CopyStreamResponse, GetCopyContextRequest, GetCopyContextResponse, RawOperation,
    ReplicationAck, ReplicationItem,
};
use crate::types::{Epoch, Lsn, Operation};

/// Secondary-side data server. Runs as a gRPC server on the data plane.
///
/// Handles:
/// - **ReplicationStream**: incremental replication from primary
/// - **GetCopyContext**: returns secondary's state context during build
/// - **CopyStream**: receives full state from primary during build
pub struct SecondaryReceiver {
    state: Arc<SecondaryState>,
    /// Partition-level state for committed_lsn propagation (B5).
    partition_state: Option<Arc<crate::handles::PartitionState>>,
    /// If set, incoming replication items are forwarded to the user's
    /// OperationStream (persisted mode). Otherwise auto-ACK (volatile).
    operation_tx: Option<mpsc::Sender<Operation>>,
    /// Sender for copy stream data. Primary pushes copy data here during build.
    copy_stream_tx: Option<Mutex<Option<mpsc::Sender<Operation>>>>,
    /// State provider channel for GetCopyContext callback.
    state_provider_tx: Option<mpsc::UnboundedSender<StateProviderEvent>>,
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

    pub fn epoch(&self) -> Epoch {
        *self.current_epoch.lock().unwrap()
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

        drop(epoch);

        let mut log = self.log.lock().unwrap();
        log.insert(item.lsn, bytes::Bytes::copy_from_slice(&item.data));

        let prev = self.received_lsn.load(Ordering::Acquire);
        if item.lsn > prev {
            self.received_lsn.store(item.lsn, Ordering::Release);
        }

        // Update committed_lsn from primary's progress (B5 fix).
        // The primary piggybacks its quorum-committed LSN on each item.
        if item.committed_lsn > self.committed_lsn.load(Ordering::Acquire) {
            self.committed_lsn
                .store(item.committed_lsn, Ordering::Release);
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
    /// Create in volatile mode (auto-ACK, backward compatible with tests).
    pub fn new(state: Arc<SecondaryState>) -> Self {
        Self {
            state,
            partition_state: None,
            operation_tx: None,
            copy_stream_tx: None,
            state_provider_tx: None,
        }
    }

    /// Create in persisted mode with full copy support.
    pub fn with_streams(
        state: Arc<SecondaryState>,
        partition_state: Arc<crate::handles::PartitionState>,
        operation_tx: mpsc::Sender<Operation>,
        copy_stream_tx: mpsc::Sender<Operation>,
        state_provider_tx: mpsc::UnboundedSender<StateProviderEvent>,
    ) -> Self {
        Self {
            state,
            partition_state: Some(partition_state),
            operation_tx: Some(operation_tx),
            copy_stream_tx: Some(Mutex::new(Some(copy_stream_tx))),
            state_provider_tx: Some(state_provider_tx),
        }
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
        let partition_state = self.partition_state.clone();
        let (ack_tx, ack_rx) = mpsc::channel(256);
        let operation_tx = self.operation_tx.clone();

        tokio::spawn(async move {
            while let Some(result) = inbound.next().await {
                match result {
                    Ok(item) => {
                        let lsn = item.lsn;
                        match state.accept_item(&item) {
                            Ok(()) => {
                                debug!(lsn, "accepted replication item");

                                // Propagate committed_lsn to PartitionState (B5 fix).
                                // This makes committed_lsn visible to PodRuntime's
                                // handle_update_epoch for correct rollback boundaries.
                                if let Some(ref ps) = partition_state
                                    && item.committed_lsn > ps.committed_lsn()
                                {
                                    ps.set_committed_lsn(item.committed_lsn);
                                }

                                if let Some(ref op_tx) = operation_tx {
                                    // Persisted mode: forward to user, defer ACK
                                    let (user_ack_tx, user_ack_rx) =
                                        tokio::sync::oneshot::channel();
                                    let op = Operation::new(
                                        lsn,
                                        bytes::Bytes::copy_from_slice(&item.data),
                                        Some(user_ack_tx),
                                    );
                                    if op_tx.send(op).await.is_err() {
                                        warn!(lsn, "operation stream closed");
                                        break;
                                    }
                                    let ack_tx = ack_tx.clone();
                                    tokio::spawn(async move {
                                        if user_ack_rx.await.is_ok() {
                                            let _ = ack_tx.send(Ok(ReplicationAck { lsn })).await;
                                        }
                                    });
                                } else {
                                    // Volatile mode: auto-ACK
                                    if ack_tx.send(Ok(ReplicationAck { lsn })).await.is_err() {
                                        break;
                                    }
                                }
                            }
                            Err(status) => {
                                warn!(
                                    lsn,
                                    error = %status.message(),
                                    "rejected replication item"
                                );
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

    async fn get_copy_context(
        &self,
        _request: Request<GetCopyContextRequest>,
    ) -> Result<Response<GetCopyContextResponse>, Status> {
        let Some(ref sp_tx) = self.state_provider_tx else {
            // No state provider — return empty context (volatile mode)
            return Ok(Response::new(GetCopyContextResponse { operations: vec![] }));
        };

        // Ask user's state provider for copy context
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        sp_tx
            .send(StateProviderEvent::GetCopyContext { reply: reply_tx })
            .map_err(|_| Status::internal("state provider closed"))?;

        let mut stream = reply_rx
            .await
            .map_err(|_| Status::internal("state provider reply dropped"))?
            .map_err(|e| Status::internal(e.to_string()))?;

        // Collect stream into response
        let mut ops = Vec::new();
        while let Some(op) = stream.get_operation().await {
            ops.push(RawOperation {
                lsn: op.lsn,
                data: op.data.to_vec(),
            });
            op.acknowledge();
        }

        info!(count = ops.len(), "GetCopyContext: sent context");
        Ok(Response::new(GetCopyContextResponse { operations: ops }))
    }

    async fn copy_stream(
        &self,
        request: Request<Streaming<CopyItem>>,
    ) -> Result<Response<CopyStreamResponse>, Status> {
        // Take the copy_stream_tx (one-time use — copy happens once)
        let tx = self
            .copy_stream_tx
            .as_ref()
            .and_then(|m| m.lock().unwrap().take())
            .ok_or_else(|| {
                Status::failed_precondition("copy stream not available or already used")
            })?;

        let mut inbound = request.into_inner();
        let mut count: i64 = 0;

        while let Some(result) = inbound.next().await {
            match result {
                Ok(item) => {
                    let op = Operation::new(item.lsn, bytes::Bytes::from(item.data), None);
                    if tx.send(op).await.is_err() {
                        warn!("copy stream receiver closed");
                        break;
                    }
                    count += 1;
                }
                Err(e) => {
                    warn!(error = %e, "copy stream error");
                    return Err(Status::internal(e.to_string()));
                }
            }
        }

        // Drop sender to signal end of copy stream
        drop(tx);
        info!(count, "CopyStream: received all copy data");

        Ok(Response::new(CopyStreamResponse {
            items_received: count,
        }))
    }
}
