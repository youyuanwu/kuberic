//! SQL RPCs use asynchronous v2 access checks and one serialized connection.
use std::sync::Arc;

use tonic::{Request, Response, Status};

use crate::connection::Statement;
use crate::proto;
use crate::service::SqliteService;

#[derive(Clone)]
pub struct SqliteServer {
    application: Arc<SqliteService>,
}

impl SqliteServer {
    pub fn new(application: Arc<SqliteService>) -> Self {
        Self { application }
    }

    async fn write(&self, statements: Vec<Statement>) -> Result<(Vec<usize>, i64, i64), Status> {
        let execution = self
            .application
            .execute(statements)
            .await
            .map_err(unavailable)?;
        Ok((execution.rows, execution.rowid, execution.lsn))
    }
}

fn unavailable(error: kuberic_runtime::RuntimeError) -> Status {
    match error {
        kuberic_runtime::RuntimeError::AdmittedOperationFailed { .. } => {
            Status::unknown(error.to_string())
        }
        error => Status::unavailable(error.to_string()),
    }
}

#[tonic::async_trait]
impl proto::sqlite_store_server::SqliteStore for SqliteServer {
    async fn execute(
        &self,
        request: Request<proto::ExecuteRequest>,
    ) -> Result<Response<proto::ExecuteResponse>, Status> {
        let request = request.into_inner();
        let (rows, rowid, lsn) = self
            .write(vec![(request.sql, convert_params(&request.params))])
            .await?;
        Ok(Response::new(proto::ExecuteResponse {
            rows_affected: rows[0] as i64,
            last_insert_rowid: rowid,
            lsn,
        }))
    }

    async fn execute_batch(
        &self,
        request: Request<proto::ExecuteBatchRequest>,
    ) -> Result<Response<proto::ExecuteBatchResponse>, Status> {
        let statements = request
            .into_inner()
            .statements
            .into_iter()
            .map(|sql| (sql, Vec::new()))
            .collect();
        let (rows, _, lsn) = self.write(statements).await?;
        Ok(Response::new(proto::ExecuteBatchResponse {
            rows_affected: rows.into_iter().map(|n| n as i64).collect(),
            lsn,
        }))
    }

    async fn query(
        &self,
        request: Request<proto::QueryRequest>,
    ) -> Result<Response<proto::QueryResponse>, Status> {
        let request = request.into_inner();
        let (columns, rows) = self
            .application
            .query(request.sql, convert_params(&request.params))
            .await
            .map_err(unavailable)?;
        Ok(Response::new(proto::QueryResponse {
            columns,
            rows: rows
                .into_iter()
                .map(|values| proto::Row {
                    values: values.into_iter().map(proto_value).collect(),
                })
                .collect(),
        }))
    }
}

fn convert_params(params: &[proto::Value]) -> Vec<rusqlite::types::Value> {
    params
        .iter()
        .map(|value| {
            if value.is_null {
                return rusqlite::types::Value::Null;
            }
            match &value.kind {
                Some(proto::value::Kind::IntegerValue(i)) => rusqlite::types::Value::Integer(*i),
                Some(proto::value::Kind::RealValue(f)) => rusqlite::types::Value::Real(*f),
                Some(proto::value::Kind::TextValue(s)) => rusqlite::types::Value::Text(s.clone()),
                Some(proto::value::Kind::BlobValue(b)) => rusqlite::types::Value::Blob(b.clone()),
                None => rusqlite::types::Value::Null,
            }
        })
        .collect()
}

fn proto_value(value: rusqlite::types::Value) -> proto::Value {
    let kind = match value {
        rusqlite::types::Value::Null => None,
        rusqlite::types::Value::Integer(i) => Some(proto::value::Kind::IntegerValue(i)),
        rusqlite::types::Value::Real(f) => Some(proto::value::Kind::RealValue(f)),
        rusqlite::types::Value::Text(s) => Some(proto::value::Kind::TextValue(s)),
        rusqlite::types::Value::Blob(b) => Some(proto::value::Kind::BlobValue(b)),
    };
    proto::Value {
        is_null: kind.is_none(),
        kind,
    }
}
