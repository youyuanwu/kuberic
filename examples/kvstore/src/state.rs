use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use kubelicate_core::types::{CancellationToken, Lsn, OperationStream};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufWriter};
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

use crate::persistence;

#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum KvOp {
    Put { key: String, value: String },
    Delete { key: String },
}

pub struct KvState {
    pub data: HashMap<String, String>,
    pub last_applied_lsn: Lsn,
    pub committed_lsn: Lsn,
    wal_writer: BufWriter<tokio::fs::File>,
    data_dir: PathBuf,
}

impl KvState {
    /// Open with persistence. Loads snapshot + replays WAL.
    /// This is the only constructor.
    pub async fn open(data_dir: PathBuf) -> std::io::Result<Self> {
        tokio::fs::create_dir_all(&data_dir).await?;
        let (data, snapshot_lsn) = persistence::load_snapshot(&data_dir).await?;

        let mut state = Self {
            data,
            last_applied_lsn: snapshot_lsn,
            committed_lsn: snapshot_lsn,
            // Temporary — replaced after replay
            wal_writer: persistence::open_wal_append(&data_dir).await?,
            data_dir: data_dir.clone(),
        };

        persistence::replay_wal(&mut state, &data_dir).await?;
        persistence::truncate_wal_to_valid(&data_dir).await?;

        // Reopen WAL for clean append (after truncation)
        state.wal_writer = persistence::open_wal_append(&data_dir).await?;
        Ok(state)
    }

    /// Apply an operation to in-memory state AND persist to WAL.
    /// Returns Err if WAL write fails — caller must NOT acknowledge.
    pub async fn apply_op(&mut self, lsn: Lsn, op: &KvOp) -> std::io::Result<()> {
        self.apply_op_in_memory(lsn, op);

        // Persist to WAL
        let entry = persistence::WalEntry {
            lsn,
            op: op.clone(),
        };
        let line = serde_json::to_string(&entry).map_err(std::io::Error::other)?;
        self.wal_writer.write_all(line.as_bytes()).await?;
        self.wal_writer.write_all(b"\n").await?;
        self.wal_writer.flush().await?;
        // fdatasync for power-failure durability. Disabled by default
        // for performance (process-crash safe via flush alone).
        // Enable with: self.wal_writer.get_ref().sync_data().await?;
        Ok(())
    }

    /// Apply an operation to in-memory state only (no WAL write).
    /// Used during WAL replay and copy stream processing.
    pub fn apply_op_in_memory(&mut self, lsn: Lsn, op: &KvOp) {
        match op {
            KvOp::Put { key, value } => {
                self.data.insert(key.clone(), value.clone());
            }
            KvOp::Delete { key } => {
                self.data.remove(key);
            }
        }
        if lsn > self.last_applied_lsn {
            self.last_applied_lsn = lsn;
        }
    }

    /// Update committed_lsn when the replicator confirms advancement.
    pub fn set_committed_lsn(&mut self, lsn: Lsn) {
        if lsn > self.committed_lsn {
            self.committed_lsn = lsn;
        }
    }

    /// Write a snapshot and truncate the WAL.
    /// Only safe when committed_lsn == last_applied_lsn.
    pub async fn checkpoint(&mut self) -> std::io::Result<()> {
        if self.committed_lsn < self.last_applied_lsn {
            warn!(
                committed = self.committed_lsn,
                applied = self.last_applied_lsn,
                "skipping checkpoint: uncommitted ops present"
            );
            return Ok(());
        }

        persistence::write_checkpoint(self, &self.data_dir.clone()).await?;

        // Truncate WAL
        let wal_path = self.data_dir.join("wal.log");
        let file = tokio::fs::File::create(&wal_path).await?;
        self.wal_writer = BufWriter::new(file);
        Ok(())
    }

    /// Rollback state to target_lsn by reloading snapshot + partial WAL replay.
    /// Call AFTER cancelling drain tasks to avoid races.
    pub async fn rollback_to(&mut self, target_lsn: Lsn) -> std::io::Result<()> {
        let dir = self.data_dir.clone();

        // Reload from snapshot (always at committed_lsn)
        let (data, snapshot_lsn) = persistence::load_snapshot(&dir).await?;
        self.data = data;
        self.last_applied_lsn = snapshot_lsn;

        // Replay WAL only up to target_lsn
        let wal_path = dir.join("wal.log");
        if let Ok(file) = tokio::fs::File::open(&wal_path).await {
            let mut lines = tokio::io::BufReader::new(file).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if let Ok(entry) = serde_json::from_str::<persistence::WalEntry>(&line)
                    && entry.lsn > self.last_applied_lsn
                    && entry.lsn <= target_lsn
                {
                    self.apply_op_in_memory(entry.lsn, &entry.op);
                }
            }
        }

        // Rewrite WAL with only entries up to target_lsn
        self.wal_writer = persistence::rewrite_wal_up_to(&dir, target_lsn).await?;

        info!(lsn = self.last_applied_lsn, "rollback complete");
        Ok(())
    }

    /// Get the data directory path.
    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }
}

use std::path::Path;

pub type SharedState = Arc<RwLock<KvState>>;

/// Drain a copy or replication stream, applying each operation to shared state.
/// Stops when the stream ends or the cancellation token fires.
///
/// Each operation is applied to in-memory state AND persisted to WAL before
/// acknowledging. The acknowledge gates the secondary's ACK back to the
/// primary, so WAL persistence is on the quorum path.
pub async fn drain_stream(
    state: SharedState,
    mut stream: OperationStream,
    token: CancellationToken,
    label: &'static str,
) {
    loop {
        tokio::select! {
            biased;
            _ = token.cancelled() => {
                info!(label, "stream drain cancelled");
                break;
            }
            item = stream.get_operation() => {
                let Some(op) = item else { break };
                let lsn = op.lsn;
                match serde_json::from_slice::<KvOp>(&op.data) {
                    Ok(kv_op) => {
                        if let Err(e) = state.write().await.apply_op(lsn, &kv_op).await {
                            warn!(lsn, error = %e, label, "WAL write failed, not acknowledging");
                            continue;
                        }
                        debug!(lsn, ?kv_op, label, "applied from stream");
                        op.acknowledge();
                    }
                    Err(e) => {
                        warn!(lsn, error = %e, label, "failed to deserialize stream op");
                    }
                }
            }
        }
    }
    info!(label, "stream drained");
}
