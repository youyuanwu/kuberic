//! Real service adapters for the opt-in physical retained-member witness.

use std::sync::Arc;

use anyhow::{Context, Result};
use async_trait::async_trait;
use kuberic_controller::test_util::{ApplicationFactory, CreatedApplication, ReplicaContext};
use kuberic_runtime::engine::DurableState;
use kuberic_runtime::host::ApplicationStorageState;
use kvstore2::{persistence::KvPersistence, service::KvService, state::CopyGate};
use rusqlite::OptionalExtension;
use serde_json::{Value, json};
use sqlite_replicated::proto::sqlite_store_server::SqliteStore;
use sqlite_replicated::{SqlitePersistence, proto, server::SqliteServer, service::SqliteService};

use crate::retained_witness::WitnessApplication;

const KEY: &str = "retained-secondary-value";

pub struct KvFactory;

#[async_trait]
impl ApplicationFactory for KvFactory {
    type Service = KvService;

    async fn create(
        &self,
        context: ReplicaContext,
    ) -> kuberic_controller::test_util::Result<CreatedApplication<Self::Service>> {
        let root = context.application_root().to_owned();
        let fresh = KvPersistence::is_fresh_empty(&root).map_err(anyhow::Error::new)?;
        let persistence = Arc::new(KvPersistence::open(&root).map_err(anyhow::Error::new)?);
        Ok(CreatedApplication::new(
            Arc::new(KvService::new(
                persistence,
                context.replication_address().to_owned(),
                CopyGate::enabled(root.join("copy-gate"))?,
            )),
            if fresh {
                ApplicationStorageState::FreshEmpty
            } else {
                ApplicationStorageState::Established
            },
        )
        .with_storage_paths([root]))
    }

    async fn shutdown_and_drain(
        &self,
        service: Arc<Self::Service>,
    ) -> kuberic_controller::test_util::Result<()> {
        service
            .shutdown_and_drain()
            .await
            .map_err(anyhow::Error::new)?;
        Ok(())
    }

    async fn member_data(
        &self,
        service: Arc<Self::Service>,
        request: Value,
    ) -> kuberic_controller::test_util::Result<Value> {
        let result: Result<Value> = async {
            if let Some(value) = request.get("set").and_then(Value::as_i64) {
                let lsn = service.replicate_put(KEY.into(), value.to_string()).await?;
                Ok(json!({"lsn": lsn}))
            } else {
                anyhow::ensure!(request == json!({"read": true}), "unknown KV request");
                let value = service
                    .get(KEY)
                    .await?
                    .context("retained KV value missing")?
                    .parse::<i64>()?;
                Ok(json!({"value": value}))
            }
        }
        .await;
        result.map_err(Into::into)
    }

    fn member_observation(
        &self,
        service: &Self::Service,
    ) -> kuberic_controller::test_util::Result<Value> {
        // The current durable-progress implementation is synchronous; this does
        // not run a host goal, service write, or a fixture provider advancement.
        let progress = futures::executor::block_on(service.persistence().durable_progress())
            .map_err(anyhow::Error::new)?;
        let snapshot = service
            .persistence()
            .snapshot_at(progress.committed_lsn)
            .map_err(anyhow::Error::new)?;
        let value = snapshot
            .get(KEY)
            .map(|value| value.parse::<i64>())
            .transpose()
            .map_err(anyhow::Error::new)?
            .unwrap_or(0);
        Ok(json!({
            "committedValue": value, "committedLsn": progress.committed_lsn,
            "appliedLsn": progress.applied_lsn,
        }))
    }
}

impl WitnessApplication for KvFactory {
    fn write_request(value: i64) -> Value {
        json!({"set": value})
    }
    fn read_request() -> Value {
        json!({"read": true})
    }
    fn write_lsn(response: Value) -> Result<i64> {
        response["lsn"]
            .as_i64()
            .context("KV did not acknowledge a native LSN")
    }
    fn read_value(response: Value) -> Result<i64> {
        response["value"]
            .as_i64()
            .context("KV serving response has no value")
    }
}

pub struct SqliteFactory;

#[async_trait]
impl ApplicationFactory for SqliteFactory {
    type Service = SqliteService;

    async fn create(
        &self,
        context: ReplicaContext,
    ) -> kuberic_controller::test_util::Result<CreatedApplication<Self::Service>> {
        let root = context.application_root().to_owned();
        let fresh = SqlitePersistence::is_fresh_empty(&root)?;
        Ok(CreatedApplication::new(
            Arc::new(SqliteService::deferred(
                root.clone(),
                context.replication_address().to_owned(),
            )?),
            if fresh {
                ApplicationStorageState::FreshEmpty
            } else {
                ApplicationStorageState::Established
            },
        )
        .with_storage_paths([root]))
    }

    async fn shutdown_and_drain(
        &self,
        service: Arc<Self::Service>,
    ) -> kuberic_controller::test_util::Result<()> {
        service
            .shutdown_and_drain()
            .await
            .map_err(anyhow::Error::new)?;
        Ok(())
    }

    async fn member_data(
        &self,
        service: Arc<Self::Service>,
        request: Value,
    ) -> kuberic_controller::test_util::Result<Value> {
        let result: Result<Value> = async {
            let server = SqliteServer::new(service);
            if let Some(sql) = request.get("sql").and_then(Value::as_str) {
                let response = server
                    .execute(tonic::Request::new(proto::ExecuteRequest {
                        sql: sql.to_owned(),
                        params: Vec::new(),
                    }))
                    .await?
                    .into_inner();
                Ok(json!({"lsn": response.lsn}))
            } else {
                anyhow::ensure!(request == json!({"query": true}), "unknown SQLite request");
                let response = server
                    .query(tonic::Request::new(proto::QueryRequest {
                        sql: "SELECT value FROM retained_witness WHERE id = 1".into(),
                        params: Vec::new(),
                    }))
                    .await?
                    .into_inner();
                let value = response
                    .rows
                    .first()
                    .and_then(|row| row.values.first())
                    .and_then(|value| value.kind.as_ref())
                    .context("SQLite row missing")?;
                let proto::value::Kind::IntegerValue(value) = value else {
                    anyhow::bail!("SQLite returned a noninteger retained value");
                };
                Ok(json!({"value": value}))
            }
        }
        .await;
        result.map_err(Into::into)
    }

    fn member_observation(
        &self,
        service: &Self::Service,
    ) -> kuberic_controller::test_util::Result<Value> {
        let result: Result<Value> = (|| {
            let inspection = service.persistence().inspect_for_test()?;
            let value = sqlite_image_value(&inspection.image)?;
            Ok(json!({
                "committedValue": value, "committedLsn": inspection.progress.committed_lsn,
                "appliedLsn": inspection.progress.applied_lsn,
                "imageSha256": format!("{:x}", bytes::Bytes::copy_from_slice(&<sha2::Sha256 as sha2::Digest>::digest(&inspection.image))),
                "streamCalls": service.persistence().stream_calls_for_test(),
            }))
        })();
        result.map_err(Into::into)
    }
}

fn sqlite_image_value(image: &[u8]) -> Result<i64> {
    // SQLite's read-only deserialize API keeps the diagnostic entirely in RAM;
    // it never opens another application writer or installs a source snapshot.
    let mut diagnostic_image = image.to_vec();
    // SQLite cannot deserialize a WAL-mode header in memory. As in the
    // existing durable-image validator, change only this diagnostic copy's
    // journal-mode bytes; neither application pages nor their raw hash change.
    if diagnostic_image.get(18..20) == Some(&[2, 2]) {
        diagnostic_image[18..20].copy_from_slice(&[1, 1]);
    }
    let mut connection = rusqlite::Connection::open_in_memory()?;
    connection.deserialize_read_exact(
        rusqlite::MAIN_DB,
        diagnostic_image.as_slice(),
        diagnostic_image.len(),
        true,
    )?;
    let has_table: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE name = 'retained_witness' AND type = 'table')",
        [],
        |row| row.get(0),
    )?;
    if !has_table {
        return Ok(0);
    }
    Ok(connection
        .query_row(
            "SELECT value FROM retained_witness WHERE id = 1",
            [],
            |row| row.get(0),
        )
        .optional()?
        .unwrap_or(0))
}

impl WitnessApplication for SqliteFactory {
    fn initialize_request() -> Option<Value> {
        Some(
            json!({"sql": "CREATE TABLE retained_witness (id INTEGER PRIMARY KEY, value INTEGER NOT NULL)"}),
        )
    }
    fn write_request(value: i64) -> Value {
        json!({"sql": format!(
            "INSERT INTO retained_witness(id, value) VALUES (1, {value}) ON CONFLICT(id) DO UPDATE SET value = excluded.value"
        )})
    }
    fn read_request() -> Value {
        json!({"query": true})
    }
    fn write_lsn(response: Value) -> Result<i64> {
        response["lsn"]
            .as_i64()
            .context("SQLite did not acknowledge a native LSN")
    }
    fn read_value(response: Value) -> Result<i64> {
        response["value"]
            .as_i64()
            .context("SQLite serving response has no value")
    }
}
