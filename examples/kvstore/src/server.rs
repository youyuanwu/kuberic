use std::sync::Arc;

use bytes::Bytes;
use kuberic_core::handles::{PartitionHandle, StateReplicatorHandle};
use kuberic_core::types::{AccessStatus, CancellationToken};
use tonic::{Request, Response, Status};
use tracing::{debug, info, warn};

use crate::proto;
use crate::state::{KvOp, SharedState};

pub struct KvServer {
    pub state: SharedState,
    pub partition: Arc<PartitionHandle>,
    pub replicator: StateReplicatorHandle,
    pub token: CancellationToken,
}

#[tonic::async_trait]
impl proto::kv_store_server::KvStore for KvServer {
    async fn get(
        &self,
        request: Request<proto::GetRequest>,
    ) -> Result<Response<proto::GetResponse>, Status> {
        match self.partition.read_status() {
            AccessStatus::Granted => {}
            AccessStatus::NotPrimary => {
                return Err(Status::unavailable("not primary — redirect to primary"));
            }
            AccessStatus::ReconfigurationPending => {
                return Err(Status::unavailable("reconfiguration in progress"));
            }
            AccessStatus::NoWriteQuorum => {
                // Reads still OK on primary without write quorum
            }
        }

        let key = &request.get_ref().key;
        let state = self.state.read().await;
        match state.data.get(key) {
            Some(value) => Ok(Response::new(proto::GetResponse {
                found: true,
                value: value.clone(),
            })),
            None => Ok(Response::new(proto::GetResponse {
                found: false,
                value: String::new(),
            })),
        }
    }

    async fn put(
        &self,
        request: Request<proto::PutRequest>,
    ) -> Result<Response<proto::PutResponse>, Status> {
        let req = request.into_inner();
        let op = KvOp::Put {
            key: req.key,
            value: req.value,
        };
        let data = serde_json::to_vec(&op).map_err(|e| Status::internal(e.to_string()))?;

        let lsn = self
            .replicator
            .replicate(Bytes::from(data), self.token.clone())
            .await
            .map_err(|e| Status::unavailable(e.to_string()))?;

        self.state
            .write()
            .await
            .apply_op(lsn, &op)
            .await
            .map_err(|e| Status::internal(format!("WAL write failed: {e}")))?;

        debug!(lsn, ?op, "replicated + applied");
        Ok(Response::new(proto::PutResponse { lsn }))
    }

    async fn delete(
        &self,
        request: Request<proto::DeleteRequest>,
    ) -> Result<Response<proto::DeleteResponse>, Status> {
        let key = request.into_inner().key;

        let existed = self.state.read().await.data.contains_key(&key);
        let op = KvOp::Delete { key };
        let data = serde_json::to_vec(&op).map_err(|e| Status::internal(e.to_string()))?;

        let lsn = self
            .replicator
            .replicate(Bytes::from(data), self.token.clone())
            .await
            .map_err(|e| Status::unavailable(e.to_string()))?;

        self.state
            .write()
            .await
            .apply_op(lsn, &op)
            .await
            .map_err(|e| Status::internal(format!("WAL write failed: {e}")))?;

        debug!(lsn, ?op, "replicated + applied");
        Ok(Response::new(proto::DeleteResponse { existed, lsn }))
    }
}

/// Start the client-facing KV gRPC server.
pub async fn run_client_server(
    bind: String,
    state: SharedState,
    partition: Arc<PartitionHandle>,
    replicator: StateReplicatorHandle,
    token: CancellationToken,
    shutdown: CancellationToken,
) {
    let listener = match tokio::net::TcpListener::bind(&bind).await {
        Ok(l) => l,
        Err(e) => {
            warn!(error = %e, "failed to bind client server");
            return;
        }
    };
    let addr = listener.local_addr().unwrap();
    info!(%addr, "client KV gRPC server started");

    let server = KvServer {
        state,
        partition,
        replicator,
        token,
    };

    let _ = tonic::transport::Server::builder()
        .add_service(proto::kv_store_server::KvStoreServer::new(server))
        .serve_with_incoming_shutdown(
            tokio_stream::wrappers::TcpListenerStream::new(listener),
            shutdown.cancelled(),
        )
        .await;

    info!("client KV gRPC server stopped");
}
