pub mod actor;
pub(crate) mod copy;
pub mod primary;
pub mod queue;
pub mod quorum;
pub mod secondary;

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{mpsc, oneshot};

use crate::error::{KubelicateError, Result};
use crate::events::{ReplicatorControlEvent, StateProviderEvent};
use crate::handles::{PartitionHandle, PartitionState, StateReplicatorHandle};
use crate::types::{CancellationToken, FaultType, OpenMode, OperationStream, ReplicaId};

// ---------------------------------------------------------------------------
// ReplicatorHandle — returned by user to runtime at Open
// ---------------------------------------------------------------------------

/// Handle returned by the user to the runtime at Open.
/// The runtime uses this to drive replicator lifecycle via control events.
/// Contains shared PartitionState for synchronous access-status fencing.
pub struct ReplicatorHandle {
    /// Send lifecycle/config commands to the replicator's event loop.
    control_tx: mpsc::Sender<ReplicatorControlEvent>,
    /// Shared partition state (atomics). Runtime uses this for:
    /// - set_status_for_role() — access status fencing (synchronous)
    /// - GetStatus — read current_progress, committed_lsn
    state: Arc<PartitionState>,
    /// Data plane address (for operator registration).
    data_address: String,
    /// Shutdown token for immediate abort.
    shutdown: CancellationToken,
}

impl ReplicatorHandle {
    pub fn new(
        control_tx: mpsc::Sender<ReplicatorControlEvent>,
        state: Arc<PartitionState>,
        data_address: String,
        shutdown: CancellationToken,
    ) -> Self {
        Self {
            control_tx,
            state,
            data_address,
            shutdown,
        }
    }

    /// Send a control event and wait for reply.
    pub async fn send_control<T>(
        &self,
        make: impl FnOnce(oneshot::Sender<Result<T>>) -> ReplicatorControlEvent,
        timeout: Duration,
    ) -> Result<T> {
        let (tx, rx) = oneshot::channel();
        self.control_tx
            .send(make(tx))
            .await
            .map_err(|_| KubelicateError::Closed)?;
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(KubelicateError::Closed),
            Err(_) => Err(KubelicateError::Internal("replicator timeout".into())),
        }
    }

    /// Synchronous access to partition state (atomics, no channel hop).
    pub fn state(&self) -> &Arc<PartitionState> {
        &self.state
    }

    /// Data plane address for operator registration.
    pub fn data_address(&self) -> &str {
        &self.data_address
    }

    /// Cancel the replicator's shutdown token for immediate abort.
    pub fn abort(&self) {
        self.shutdown.cancel();
    }
}

// ---------------------------------------------------------------------------
// ServiceContext — user-facing handles kept by the user service
// ---------------------------------------------------------------------------

/// User-facing handles produced by the replicator factory.
/// The user keeps these; the ReplicatorHandle goes to the runtime.
pub struct ServiceContext {
    /// Write handle (primary path).
    pub replicator: StateReplicatorHandle,
    /// Read/write access status + fault reporting.
    pub partition: Arc<PartitionHandle>,
    /// Copy stream (secondary, during build). None on primary.
    pub copy_stream: Option<OperationStream>,
    /// Replication stream (secondary, during catchup). None on primary.
    pub replication_stream: Option<OperationStream>,
}

// ---------------------------------------------------------------------------
// OpenContext — provided to user at Open to create a replicator
// ---------------------------------------------------------------------------

/// Context provided to the user at Open time. Contains what the user needs
/// to create a replicator (bind addresses, replica ID, etc.)
pub struct OpenContext {
    pub replica_id: ReplicaId,
    /// New vs Existing — the replicator needs this for initialization.
    pub open_mode: OpenMode,
    /// Address for the data plane gRPC server.
    pub data_bind: String,
    /// Cancellation token for the replica's lifetime.
    pub token: CancellationToken,
    /// Fault reporting channel (runtime holds the receiver).
    pub fault_tx: mpsc::Sender<FaultType>,
}

// ---------------------------------------------------------------------------
// WalReplicator — factory for the WAL-based quorum replicator
// ---------------------------------------------------------------------------

use tonic::transport::Server;

use crate::events::ReplicateRequest;
use crate::proto::replicator_data_server::ReplicatorDataServer;
use crate::replicator::actor::WalReplicatorActor;
use crate::replicator::secondary::{SecondaryReceiver, SecondaryState};

/// WAL-based quorum replicator factory.
/// Creates the actor, data plane, streams, and returns handles.
pub struct WalReplicator;

impl WalReplicator {
    /// Create a new WalReplicator. Starts:
    /// - WalReplicatorActor (event loop processing control_rx + data_rx)
    /// - Data plane gRPC server (SecondaryReceiver)
    /// - Replication + copy streams
    ///
    /// Returns (runtime_handle, user_handles):
    /// - runtime_handle: for runtime to send lifecycle events
    /// - user_handles: for user to read/write replicated data
    pub async fn create(
        replica_id: ReplicaId,
        data_bind: &str,
        fault_tx: mpsc::Sender<FaultType>,
        state_provider_tx: mpsc::UnboundedSender<StateProviderEvent>,
    ) -> Result<(ReplicatorHandle, ServiceContext)> {
        let (control_tx, control_rx) = mpsc::channel(16);
        let (data_tx, data_rx) = mpsc::channel::<ReplicateRequest>(256);
        let state = Arc::new(PartitionState::new());
        let secondary_state = Arc::new(SecondaryState::new());
        let shutdown = CancellationToken::new();

        // Replication + copy streams
        let (repl_op_tx, repl_op_rx) = mpsc::channel(256);
        let replication_stream = OperationStream::new(repl_op_rx);
        let (copy_op_tx, copy_op_rx) = mpsc::channel(256);
        let copy_stream = OperationStream::new(copy_op_rx);

        // Data plane gRPC server
        let data_receiver = SecondaryReceiver::with_streams(
            secondary_state,
            state.clone(),
            repl_op_tx,
            copy_op_tx,
            state_provider_tx.clone(),
        );
        let data_listener = tokio::net::TcpListener::bind(data_bind)
            .await
            .map_err(|e| KubelicateError::Internal(Box::new(e)))?;
        let data_addr = data_listener.local_addr().unwrap();
        let data_address = format!("http://{}", data_addr);

        let data_shutdown = shutdown.child_token();
        tokio::spawn(async move {
            let _ = Server::builder()
                .add_service(ReplicatorDataServer::new(data_receiver))
                .serve_with_incoming_shutdown(
                    tokio_stream::wrappers::TcpListenerStream::new(data_listener),
                    data_shutdown.cancelled(),
                )
                .await;
        });

        // Replicator actor — receives state_provider_tx for forwarding
        let actor = WalReplicatorActor::new(replica_id);
        let state_cp = state.clone();
        let sp_tx = state_provider_tx.clone();
        tokio::spawn(async move {
            actor.run(control_rx, data_rx, state_cp, sp_tx).await;
        });

        // Build handles
        let partition = Arc::new(PartitionHandle::new(state.clone(), fault_tx));
        let replicator_write = StateReplicatorHandle::new(data_tx, state.clone());

        let runtime_handle =
            ReplicatorHandle::new(control_tx, state.clone(), data_address, shutdown);

        let user_handles = ServiceContext {
            replicator: replicator_write,
            partition,
            copy_stream: Some(copy_stream),
            replication_stream: Some(replication_stream),
        };

        Ok((runtime_handle, user_handles))
    }
}
