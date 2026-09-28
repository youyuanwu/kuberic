//! SqliteState: manages SQLite database for both primary and secondary roles.
//!
//! Primary: opens SQLite in WAL mode, captures WAL frames after commits.
//! Secondary: applies received page data directly to the DB file.

use std::io;
use std::path::PathBuf;
use std::sync::Arc;

use kuberic_core::types::Lsn;
use rusqlite::{Connection, OpenFlags};
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

use crate::framelog::{FrameLog, FrameLogMeta};
use crate::frames::WalFrameSet;

pub use crate::framelog::durable::RecoveryState;
use crate::framelog::durable::{DurableFrameLog, replace, sync_directory};
use kuberic_protocol::types::OperationId;
use kuberic_runtime::application::{
    CopyChunk, DurableApplicationAck, DurableApplicationProgress, Operation,
};
use kuberic_runtime::engine::{DurableState, RetainedOperationStream};

/// Application-owned v2 persistence. The classic connection owner below remains
/// temporarily available until hosting and the commit barrier switch together.
pub struct SqlitePersistence {
    root: PathBuf,
    log: std::sync::Mutex<DurableFrameLog>,
}

impl SqlitePersistence {
    /// Open fresh-v2 storage or recover an existing manifest without importing classic data.
    pub fn open(root: PathBuf) -> io::Result<Self> {
        let log = DurableFrameLog::open(root.clone())?;
        Ok(Self {
            root,
            log: std::sync::Mutex::new(log),
        })
    }

    fn lock(&self) -> io::Result<std::sync::MutexGuard<'_, DurableFrameLog>> {
        self.log
            .lock()
            .map_err(|_| io::Error::other("SQLite persistence mutex poisoned"))
    }

    /// Inspect the durable fence; opening the directory does not clear it.
    pub fn recovery_state(&self) -> io::Result<RecoveryState> {
        Ok(self.lock()?.recovery())
    }

    /// Fence SQL after a dispatched transaction has an unresolved publication outcome.
    pub fn require_reconciliation(&self, reason: String) -> io::Result<()> {
        self.lock()?.reconcile(reason)
    }

    /// Encode the exact committed image without touching live SQL. Applied suffixes
    /// are enumerated separately through `get_replication_operations`.
    pub fn snapshot(&self, up_to_lsn: i64) -> io::Result<Vec<u8>> {
        self.lock()?.snapshot(up_to_lsn)
    }

    /// Materialize committed history with no live SQLite connection. Recovery
    /// authority must settle outstanding reservations before clearing reconciliation.
    pub fn materialize_committed(&self) -> io::Result<PathBuf> {
        let log = self.lock()?;
        self.materialize(&log)
    }

    fn materialize(&self, log: &DurableFrameLog) -> io::Result<PathBuf> {
        let image = log.image_at(log.progress().committed_lsn)?;
        for companion in ["db.sqlite-wal", "db.sqlite-shm"] {
            match std::fs::remove_file(self.root.join(companion)) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
        }
        sync_directory(&self.root)?;
        let path = self.root.join("db.sqlite");
        replace(&path, &image)?;
        Ok(path)
    }

    /// Called only after v2 has resolved exact journaled writes under accepted
    /// authority; ordinary reopen intentionally never clears either durable fence.
    pub fn complete_reconciliation(&self) -> io::Result<()> {
        let mut log = self.lock()?;
        self.materialize(&log)?;
        log.clear_reconciliation()
    }
}

fn persistence_error(error: io::Error) -> kuberic_runtime::RuntimeError {
    kuberic_runtime::RuntimeError::Application(error.to_string())
}

#[async_trait::async_trait]
impl DurableState for SqlitePersistence {
    async fn get_replication_operations(
        &self,
        from_lsn: i64,
        to_lsn: i64,
    ) -> kuberic_runtime::Result<RetainedOperationStream> {
        let operations = self
            .lock()
            .and_then(|log| log.retained(from_lsn, to_lsn))
            .map_err(persistence_error)?;
        Ok(Box::pin(futures::stream::iter(
            operations.into_iter().map(Ok),
        )))
    }

    async fn apply_copy_chunk(
        &self,
        build_id: &OperationId,
        sequence: u64,
        chunk: CopyChunk,
    ) -> kuberic_runtime::Result<()> {
        self.lock()
            .and_then(|mut log| log.stage(build_id.as_str(), sequence, chunk.data.to_vec()))
            .map_err(persistence_error)
    }

    async fn verify_copy_chunk(
        &self,
        build_id: &OperationId,
        sequence: u64,
        chunk: &CopyChunk,
    ) -> kuberic_runtime::Result<bool> {
        self.lock()
            .map(|log| log.verify_chunk(build_id.as_str(), sequence, &chunk.data))
            .map_err(persistence_error)
    }

    async fn finish_copy(
        &self,
        build_id: &OperationId,
        up_to_lsn: i64,
        committed_lsn: i64,
    ) -> kuberic_runtime::Result<DurableApplicationProgress> {
        self.lock()
            .and_then(|mut log| log.finish(build_id.as_str(), up_to_lsn, committed_lsn))
            .map_err(persistence_error)
    }

    async fn apply(&self, operation: Operation) -> kuberic_runtime::Result<DurableApplicationAck> {
        self.lock()
            .and_then(|mut log| log.apply(operation))
            .map_err(persistence_error)
    }

    async fn durable_progress(&self) -> kuberic_runtime::Result<DurableApplicationProgress> {
        self.lock()
            .map(|log| log.progress())
            .map_err(persistence_error)
    }

    async fn verify_applied(&self, operation: &Operation) -> kuberic_runtime::Result<bool> {
        self.lock()
            .and_then(|log| log.verify(operation))
            .map_err(persistence_error)
    }

    async fn commit(
        &self,
        committed_lsn: i64,
    ) -> kuberic_runtime::Result<DurableApplicationProgress> {
        self.lock()
            .and_then(|mut log| log.commit(committed_lsn))
            .map_err(persistence_error)
    }
}

/// The SQLite database state.
pub struct SqliteState {
    /// SQLite connection (only on primary).
    conn: Option<Connection>,
    /// Path to the SQLite database file.
    pub db_path: PathBuf,
    /// Data directory containing db, frames.log, meta.json.
    pub data_dir: PathBuf,
    /// SQLite page size (typically 4096).
    pub page_size: u32,
    /// Last LSN applied to the database.
    pub last_applied_lsn: Lsn,
    /// Last committed LSN (confirmed by quorum).
    pub committed_lsn: Lsn,
    /// Secondary's frame log.
    frame_log: Option<FrameLog>,
}

pub type SharedState = Arc<Mutex<SqliteState>>;

impl SqliteState {
    /// Create a new SqliteState. Does not open the database yet.
    pub async fn open(data_dir: PathBuf) -> io::Result<Self> {
        tokio::fs::create_dir_all(&data_dir).await?;
        let db_path = data_dir.join("db.sqlite");

        // Load metadata if exists
        let meta = FrameLog::load_meta(&data_dir).await?;

        Ok(Self {
            conn: None,
            db_path,
            data_dir,
            page_size: 4096,
            last_applied_lsn: meta.committed_lsn,
            committed_lsn: meta.committed_lsn,
            frame_log: None,
        })
    }

    /// Open SQLite as primary: WAL mode, single connection, no auto-checkpoint.
    ///
    /// Opened against the commit-barrier VFS, so a transaction only becomes
    /// visible once it has reached durable quorum. `locking_mode=EXCLUSIVE`
    /// keeps the wal-index in heap memory instead of a shared-memory file, and
    /// `synchronous=FULL` keeps the published commit durable on this replica.
    pub fn open_as_primary(&mut self) -> io::Result<()> {
        if crate::barrier::is_fenced_on_disk(&self.data_dir) {
            return Err(io::Error::other(
                "replica lost a replicated transaction locally and must be rebuilt",
            ));
        }
        let conn = Connection::open_with_flags_and_vfs(
            &self.db_path,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_CREATE
                | OpenFlags::SQLITE_OPEN_NO_MUTEX,
            crate::barrier::VFS_NAME,
        )
        .map_err(|e| io::Error::other(format!("failed to open SQLite: {e}")))?;

        conn.pragma_update(None, "locking_mode", "EXCLUSIVE")
            .map_err(|e| io::Error::other(format!("failed to set locking mode: {e}")))?;

        // Enable WAL mode
        conn.pragma_update(None, "journal_mode", "wal")
            .map_err(|e| io::Error::other(format!("failed to set WAL mode: {e}")))?;

        conn.pragma_update(None, "synchronous", "FULL")
            .map_err(|e| io::Error::other(format!("failed to set synchronous mode: {e}")))?;

        // Disable auto-checkpoint — we control checkpointing
        conn.pragma_update(None, "wal_autocheckpoint", 0)
            .map_err(|e| io::Error::other(format!("failed to disable auto-checkpoint: {e}")))?;

        // Read page size
        let page_size: u32 = conn
            .pragma_query_value(None, "page_size", |row| row.get(0))
            .map_err(|e| io::Error::other(format!("failed to read page_size: {e}")))?;
        self.page_size = page_size;

        info!(
            db = %self.db_path.display(),
            page_size,
            "SQLite opened as primary (WAL mode)"
        );

        self.conn = Some(conn);
        Ok(())
    }

    /// Open frame log for secondary role.
    pub async fn open_frame_log(&mut self) -> io::Result<()> {
        self.frame_log = Some(FrameLog::open(&self.data_dir).await?);
        Ok(())
    }

    /// Execute a write SQL statement (primary only).
    /// Returns (rows_affected, last_insert_rowid).
    pub fn execute_sql(
        &self,
        sql: &str,
        params: &[rusqlite::types::Value],
    ) -> Result<(usize, i64), rusqlite::Error> {
        let conn = self.conn.as_ref().expect("not open as primary");
        let rows = conn.execute(sql, rusqlite::params_from_iter(params))?;
        let rowid = conn.last_insert_rowid();
        Ok((rows, rowid))
    }

    /// Execute a batch of SQL statements in a transaction (primary only).
    /// Returns rows_affected per statement.
    pub fn execute_batch_sql(
        &mut self,
        statements: &[String],
    ) -> Result<Vec<usize>, rusqlite::Error> {
        let conn = self.conn.as_mut().expect("not open as primary");
        let tx = conn.transaction()?;
        let mut results = Vec::with_capacity(statements.len());
        for sql in statements {
            let rows = tx.execute(sql, [])?;
            results.push(rows);
        }
        tx.commit()?;
        Ok(results)
    }

    /// Execute a SELECT query (primary only).
    /// Returns (column_names, rows) where each row is a Vec of serde_json::Value.
    pub fn query_sql(
        &self,
        sql: &str,
        params: &[rusqlite::types::Value],
    ) -> Result<(Vec<String>, Vec<Vec<serde_json::Value>>), rusqlite::Error> {
        let conn = self.conn.as_ref().expect("not open as primary");
        let mut stmt = conn.prepare(sql)?;
        let columns: Vec<String> = stmt.column_names().iter().map(|s| s.to_string()).collect();
        let col_count = columns.len();

        let rows = stmt
            .query_map(rusqlite::params_from_iter(params), |row| {
                let mut values = Vec::with_capacity(col_count);
                for i in 0..col_count {
                    let val: rusqlite::types::Value = row.get(i)?;
                    values.push(sqlite_value_to_json(val));
                }
                Ok(values)
            })?
            .collect::<Result<Vec<_>, _>>()?;

        Ok((columns, rows))
    }

    pub async fn mark_confirmed(&mut self, lsn: Lsn) -> io::Result<()> {
        if lsn <= self.committed_lsn {
            return Ok(());
        }
        self.last_applied_lsn = lsn;
        self.committed_lsn = lsn;

        FrameLog::save_meta(&self.data_dir, &FrameLogMeta { committed_lsn: lsn }).await
    }

    /// Apply a WalFrameSet to the database file (secondary).
    /// Writes page data directly at the correct file offsets.
    pub fn apply_frames(&mut self, frame_set: &WalFrameSet) -> io::Result<()> {
        use std::io::{Seek, SeekFrom, Write};

        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .read(true)
            .open(&self.db_path)?;

        // Extend file if needed
        if frame_set.db_size_pages > 0 {
            let target_size = frame_set.db_size_pages as u64 * self.page_size as u64;
            let current_size = file.metadata()?.len();
            if target_size > current_size {
                file.set_len(target_size)?;
            }
        }

        for frame in &frame_set.frames {
            let offset = (frame.page_number as u64 - 1) * self.page_size as u64;
            file.seek(SeekFrom::Start(offset))?;
            file.write_all(&frame.data)?;
        }

        file.sync_data()?;

        debug!(
            num_frames = frame_set.frames.len(),
            db_size_pages = frame_set.db_size_pages,
            "applied frames to DB file"
        );

        Ok(())
    }

    /// Persist a frame set to the frame log (secondary, before ACK).
    pub async fn persist_frame(&mut self, lsn: Lsn, frame_set: &WalFrameSet) -> io::Result<()> {
        if let Some(ref mut log) = self.frame_log {
            log.append(lsn, frame_set).await?;
        }
        if lsn > self.last_applied_lsn {
            self.last_applied_lsn = lsn;
        }
        Ok(())
    }

    /// Apply committed frames from frame log to DB file (secondary).
    pub async fn apply_committed_frames(&mut self, up_to_lsn: Lsn) -> io::Result<()> {
        let entries = FrameLog::read_all(&self.data_dir).await?;
        let mut applied = 0u64;
        for entry in &entries {
            if entry.lsn <= up_to_lsn && entry.lsn > self.committed_lsn {
                self.apply_frames(&entry.frame_set)?;
                applied += 1;
            }
        }
        if applied > 0 {
            self.committed_lsn = up_to_lsn;
            FrameLog::save_meta(
                &self.data_dir,
                &FrameLogMeta {
                    committed_lsn: self.committed_lsn,
                },
            )
            .await?;
            info!(
                committed_lsn = self.committed_lsn,
                applied, "applied committed frames"
            );
        }
        Ok(())
    }

    /// Rollback: truncate frame log beyond target LSN.
    pub async fn rollback_to(&mut self, target_lsn: Lsn) -> io::Result<()> {
        if target_lsn < self.committed_lsn {
            warn!(
                target_lsn,
                committed_lsn = self.committed_lsn,
                "rollback past committed data — need full copy"
            );
            // Signal data loss needed
            return Ok(());
        }

        FrameLog::truncate_to(&self.data_dir, target_lsn).await?;
        self.last_applied_lsn = target_lsn;
        info!(target_lsn, "rollback complete");
        Ok(())
    }

    /// Produce a full snapshot of the database for copy protocol.
    pub fn snapshot_db(&self) -> io::Result<Vec<u8>> {
        if let Some(ref conn) = self.conn {
            // Checkpoint WAL into DB file so the snapshot has all data
            let _ = conn.execute_batch("PRAGMA wal_checkpoint(PASSIVE)");
        }
        std::fs::read(&self.db_path)
    }

    /// Replace DB file with a full snapshot (secondary, during copy).
    pub async fn restore_from_snapshot(&mut self, data: &[u8]) -> io::Result<()> {
        // Close any existing connection
        self.conn = None;
        tokio::fs::write(&self.db_path, data).await?;

        // Remove any stale WAL/SHM files
        let wal = self.data_dir.join("db.sqlite-wal");
        let shm = self.data_dir.join("db.sqlite-shm");
        let _ = tokio::fs::remove_file(&wal).await;
        let _ = tokio::fs::remove_file(&shm).await;

        crate::barrier::barrier().clear_fence_after_rebuild(&self.data_dir);

        info!(size = data.len(), "restored DB from snapshot");
        Ok(())
    }

    /// Close the SQLite connection.
    pub fn close(&mut self) {
        if let Some(conn) = self.conn.take() {
            let _ = conn.execute_batch("PRAGMA wal_checkpoint(PASSIVE)");
            drop(conn);
            info!("SQLite connection closed");
        }
    }
}

fn sqlite_value_to_json(val: rusqlite::types::Value) -> serde_json::Value {
    match val {
        rusqlite::types::Value::Null => serde_json::Value::Null,
        rusqlite::types::Value::Integer(i) => serde_json::Value::Number(i.into()),
        rusqlite::types::Value::Real(f) => {
            serde_json::json!(f)
        }
        rusqlite::types::Value::Text(s) => serde_json::Value::String(s),
        rusqlite::types::Value::Blob(b) => {
            let hex: String = b.iter().map(|byte| format!("{:02x}", byte)).collect();
            serde_json::Value::String(hex)
        }
    }
}
