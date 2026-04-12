//! File-based persistence for KvState.
//!
//! Two files:
//! - `state.json` — periodic full snapshot (atomic write-rename)
//! - `wal.log` — append-only NDJSON log of KvOps since last snapshot
//!
//! Recovery: load snapshot + replay WAL. Crash-safe via:
//! - WAL: append + fdatasync before ACK
//! - Snapshot: write-tmp + fdatasync + rename (atomic)
//! - WAL truncation after replay (removes corrupt trailing bytes)

use std::path::Path;

use serde::{Deserialize, Serialize};
use tokio::fs;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, BufWriter};
use tracing::{info, warn};

use crate::state::{KvOp, KvState};
use kuberic_core::types::Lsn;

/// A single WAL entry: one operation at one LSN.
#[derive(Serialize, Deserialize)]
pub struct WalEntry {
    pub lsn: Lsn,
    pub op: KvOp,
}

/// Snapshot format: full HashMap + LSN.
#[derive(Serialize, Deserialize)]
pub struct SnapshotData {
    pub last_applied_lsn: Lsn,
    pub data: std::collections::HashMap<String, String>,
}

/// Load a snapshot from disk, or return empty state if no snapshot exists.
pub async fn load_snapshot(
    dir: &Path,
) -> std::io::Result<(std::collections::HashMap<String, String>, Lsn)> {
    let path = dir.join("state.json");
    match fs::read_to_string(&path).await {
        Ok(contents) => {
            let snap: SnapshotData = serde_json::from_str(&contents)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
            info!(
                lsn = snap.last_applied_lsn,
                keys = snap.data.len(),
                "loaded snapshot"
            );
            Ok((snap.data, snap.last_applied_lsn))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            info!("no snapshot found, starting empty");
            Ok((std::collections::HashMap::new(), 0))
        }
        Err(e) => Err(e),
    }
}

/// Replay WAL entries from disk, applying only entries with LSN > current.
/// Returns the number of entries replayed.
pub async fn replay_wal(state: &mut KvState, dir: &Path) -> std::io::Result<u64> {
    let path = dir.join("wal.log");
    let file = match fs::File::open(&path).await {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(e),
    };
    let reader = BufReader::new(file);
    let mut lines = reader.lines();
    let mut replayed = 0u64;
    while let Ok(Some(line)) = lines.next_line().await {
        match serde_json::from_str::<WalEntry>(&line) {
            Ok(entry) if entry.lsn > state.last_applied_lsn => {
                // Apply directly to HashMap (no WAL write during replay)
                state.apply_op_in_memory(entry.lsn, &entry.op);
                replayed += 1;
            }
            Ok(_) => {} // Already in snapshot, skip
            Err(e) => {
                warn!(error = %e, "truncated WAL entry, stopping replay");
                break; // Crash mid-write — stop at corrupt line
            }
        }
    }
    info!(
        replayed,
        lsn = state.last_applied_lsn,
        "WAL replay complete"
    );
    Ok(replayed)
}

/// After replay, rewrite WAL with only valid entries to remove any
/// corrupt trailing bytes. Without this, new entries appended after
/// the corrupt line would be lost on next recovery.
pub async fn truncate_wal_to_valid(dir: &Path) -> std::io::Result<()> {
    let wal_path = dir.join("wal.log");
    let tmp_path = dir.join("wal.log.tmp");

    let mut valid_lines = Vec::new();
    if let Ok(file) = fs::File::open(&wal_path).await {
        let mut lines = BufReader::new(file).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if serde_json::from_str::<WalEntry>(&line).is_ok() {
                valid_lines.push(line);
            } else {
                break; // Stop at first corrupt line
            }
        }
    } else {
        return Ok(()); // No WAL file
    }

    // Write valid entries to tmp, sync, rename
    let tmp_file = fs::File::create(&tmp_path).await?;
    let mut writer = BufWriter::new(tmp_file);
    for line in &valid_lines {
        writer.write_all(line.as_bytes()).await?;
        writer.write_all(b"\n").await?;
    }
    writer.flush().await?;
    writer.get_ref().sync_data().await?;
    drop(writer);
    fs::rename(&tmp_path, &wal_path).await?;
    Ok(())
}

/// Open WAL file for appending.
pub async fn open_wal_append(dir: &Path) -> std::io::Result<BufWriter<fs::File>> {
    let file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("wal.log"))
        .await?;
    Ok(BufWriter::new(file))
}

/// Write a snapshot and truncate the WAL.
/// Only call when committed_lsn == last_applied_lsn (no uncommitted ops).
pub async fn write_checkpoint(state: &KvState, dir: &Path) -> std::io::Result<()> {
    let snapshot = SnapshotData {
        last_applied_lsn: state.last_applied_lsn,
        data: state.data.clone(),
    };

    // Atomic write: tmp → sync → rename
    let tmp = dir.join("state.json.tmp");
    let dst = dir.join("state.json");
    let json = serde_json::to_string(&snapshot).map_err(std::io::Error::other)?;
    fs::write(&tmp, &json).await?;
    // fsync snapshot before rename
    let tmp_file = fs::File::open(&tmp).await?;
    tmp_file.sync_data().await?;
    drop(tmp_file);
    fs::rename(&tmp, &dst).await?;

    info!(
        lsn = state.last_applied_lsn,
        keys = state.data.len(),
        "checkpoint written"
    );
    Ok(())
}

/// Rewrite WAL keeping only entries up to max_lsn.
/// Returns a new BufWriter for the rewritten WAL.
pub async fn rewrite_wal_up_to(dir: &Path, max_lsn: Lsn) -> std::io::Result<BufWriter<fs::File>> {
    let wal_path = dir.join("wal.log");
    let tmp_path = dir.join("wal.log.tmp");

    // Read valid entries up to max_lsn
    let mut entries = Vec::new();
    if let Ok(file) = fs::File::open(&wal_path).await {
        let mut lines = BufReader::new(file).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if let Ok(entry) = serde_json::from_str::<WalEntry>(&line)
                && entry.lsn <= max_lsn
            {
                entries.push(line);
            }
        }
    }

    // Write filtered entries, sync, rename
    let tmp_file = fs::File::create(&tmp_path).await?;
    let mut writer = BufWriter::new(tmp_file);
    for line in &entries {
        writer.write_all(line.as_bytes()).await?;
        writer.write_all(b"\n").await?;
    }
    writer.flush().await?;
    writer.get_ref().sync_data().await?;
    drop(writer);
    fs::rename(&tmp_path, &wal_path).await?;

    // Reopen for append
    open_wal_append(dir).await
}
