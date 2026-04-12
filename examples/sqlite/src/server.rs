//! Client-facing SQL gRPC server (primary only).

use std::sync::Arc;

use bytes::Bytes;
use kubelicate_core::handles::{PartitionHandle, StateReplicatorHandle};
use kubelicate_core::types::{AccessStatus, CancellationToken};
use tonic::{Request, Response, Status};
use tracing::{debug, info, warn};

use crate::proto;
use crate::state::SharedState;

pub struct SqliteServer {
    pub state: SharedState,
    pub partition: Arc<PartitionHandle>,
    pub replicator: StateReplicatorHandle,
    pub token: CancellationToken,
}

#[tonic::async_trait]
impl proto::sqlite_store_server::SqliteStore for SqliteServer {
    async fn execute(
        &self,
        request: Request<proto::ExecuteRequest>,
    ) -> Result<Response<proto::ExecuteResponse>, Status> {
        self.check_write_access()?;

        let req = request.into_inner();
        let params = convert_params(&req.params);

        // Execute SQL (this commits the transaction internally)
        let (rows_affected, last_insert_rowid) = {
            let state = self.state.lock().await;
            state
                .execute_sql(&req.sql, &params)
                .map_err(|e| Status::internal(format!("SQL error: {e}")))?
        };

        // Capture WAL frames and replicate
        let lsn = self.capture_and_replicate().await?;

        debug!(lsn, rows_affected, "execute complete");
        Ok(Response::new(proto::ExecuteResponse {
            rows_affected: rows_affected as i64,
            last_insert_rowid,
            lsn,
        }))
    }

    async fn query(
        &self,
        request: Request<proto::QueryRequest>,
    ) -> Result<Response<proto::QueryResponse>, Status> {
        self.check_read_access()?;

        let req = request.into_inner();
        let params = convert_params(&req.params);

        let state = self.state.lock().await;
        let (columns, rows) = state
            .query_sql(&req.sql, &params)
            .map_err(|e| Status::internal(format!("SQL error: {e}")))?;

        let proto_rows: Vec<proto::Row> = rows
            .into_iter()
            .map(|row| proto::Row {
                values: row.into_iter().map(json_to_proto_value).collect(),
            })
            .collect();

        Ok(Response::new(proto::QueryResponse {
            columns,
            rows: proto_rows,
        }))
    }

    async fn execute_batch(
        &self,
        request: Request<proto::ExecuteBatchRequest>,
    ) -> Result<Response<proto::ExecuteBatchResponse>, Status> {
        self.check_write_access()?;

        let req = request.into_inner();

        let rows_affected = {
            let mut state = self.state.lock().await;
            state
                .execute_batch_sql(&req.statements)
                .map_err(|e| Status::internal(format!("SQL error: {e}")))?
        };

        let lsn = self.capture_and_replicate().await?;

        debug!(lsn, statements = req.statements.len(), "batch complete");
        Ok(Response::new(proto::ExecuteBatchResponse {
            rows_affected: rows_affected.into_iter().map(|r| r as i64).collect(),
            lsn,
        }))
    }
}

impl SqliteServer {
    fn check_write_access(&self) -> Result<(), Status> {
        match self.partition.read_status() {
            AccessStatus::Granted => Ok(()),
            AccessStatus::NotPrimary => {
                Err(Status::unavailable("not primary — redirect to primary"))
            }
            AccessStatus::ReconfigurationPending => {
                Err(Status::unavailable("reconfiguration in progress"))
            }
            AccessStatus::NoWriteQuorum => Err(Status::unavailable("no write quorum available")),
        }
    }

    fn check_read_access(&self) -> Result<(), Status> {
        match self.partition.read_status() {
            AccessStatus::Granted | AccessStatus::NoWriteQuorum => Ok(()),
            AccessStatus::NotPrimary => {
                Err(Status::unavailable("not primary — redirect to primary"))
            }
            AccessStatus::ReconfigurationPending => {
                Err(Status::unavailable("reconfiguration in progress"))
            }
        }
    }

    /// Capture WAL frames from the last write and replicate to secondaries.
    async fn capture_and_replicate(&self) -> Result<i64, Status> {
        let frame_set = {
            let mut state = self.state.lock().await;
            state
                .capture_wal_frames()
                .map_err(|e| Status::internal(format!("WAL capture failed: {e}")))?
        };

        if let Some(fs) = frame_set {
            let data = serde_json::to_vec(&fs)
                .map_err(|e| Status::internal(format!("serialization failed: {e}")))?;

            let lsn = self
                .replicator
                .replicate(Bytes::from(data), self.token.clone())
                .await
                .map_err(|e| Status::unavailable(format!("replication failed: {e}")))?;

            // Update state LSN
            {
                let mut state = self.state.lock().await;
                state.last_applied_lsn = lsn;
                state.committed_lsn = lsn;
            }

            Ok(lsn)
        } else {
            // No WAL frames (no-op write like empty transaction)
            let state = self.state.lock().await;
            Ok(state.last_applied_lsn)
        }
    }
}

/// Start the client-facing SQL gRPC server.
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
    info!(%addr, "client SQL gRPC server started");

    let server = SqliteServer {
        state,
        partition,
        replicator,
        token,
    };

    let _ = tonic::transport::Server::builder()
        .add_service(proto::sqlite_store_server::SqliteStoreServer::new(server))
        .serve_with_incoming_shutdown(
            tokio_stream::wrappers::TcpListenerStream::new(listener),
            shutdown.cancelled(),
        )
        .await;

    info!("client SQL gRPC server stopped");
}

fn convert_params(params: &[proto::Value]) -> Vec<rusqlite::types::Value> {
    params
        .iter()
        .map(|v| {
            if v.is_null {
                return rusqlite::types::Value::Null;
            }
            match &v.kind {
                Some(proto::value::Kind::IntegerValue(i)) => rusqlite::types::Value::Integer(*i),
                Some(proto::value::Kind::RealValue(f)) => rusqlite::types::Value::Real(*f),
                Some(proto::value::Kind::TextValue(s)) => rusqlite::types::Value::Text(s.clone()),
                Some(proto::value::Kind::BlobValue(b)) => rusqlite::types::Value::Blob(b.clone()),
                None => rusqlite::types::Value::Null,
            }
        })
        .collect()
}

fn json_to_proto_value(val: serde_json::Value) -> proto::Value {
    match val {
        serde_json::Value::Null => proto::Value {
            kind: None,
            is_null: true,
        },
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                proto::Value {
                    kind: Some(proto::value::Kind::IntegerValue(i)),
                    is_null: false,
                }
            } else if let Some(f) = n.as_f64() {
                proto::Value {
                    kind: Some(proto::value::Kind::RealValue(f)),
                    is_null: false,
                }
            } else {
                proto::Value {
                    kind: None,
                    is_null: true,
                }
            }
        }
        serde_json::Value::String(s) => proto::Value {
            kind: Some(proto::value::Kind::TextValue(s)),
            is_null: false,
        },
        _ => proto::Value {
            kind: Some(proto::value::Kind::TextValue(val.to_string())),
            is_null: false,
        },
    }
}
