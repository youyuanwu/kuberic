# KV-Stateful: Replicated Key-Value Store Example

Example application demonstrating kubelicate-core's full replication protocol.
A gRPC key-value store that replicates writes via quorum, serves reads from
local state, handles copy/replication streams on secondaries, and implements
all StateProvider callbacks.

---

## Architecture

```
Clients (gRPC)
    │
    ▼
┌──────────────────────────────────────────────────────┐
│                    kv-stateful Pod                     │
│                                                        │
│  ┌──────────────┐    ┌─────────────────────────────┐  │
│  │  PodRuntime   │    │  KV Service (user app)      │  │
│  │               │    │                             │  │
│  │ control gRPC ◄┤    │  ┌───────────────────────┐  │  │
│  │ data    gRPC ◄┤    │  │  KvState              │  │  │
│  │               │    │  │  HashMap<String,String>│  │  │
│  │ lifecycle_rx ─┼───►│  │  last_applied_lsn     │  │  │
│  │ state_prov_rx─┼───►│  └───────────────────────┘  │  │
│  │               │    │                             │  │
│  └──────────────┘    │  client gRPC ◄── Clients    │  │
│                       └─────────────────────────────┘  │
└──────────────────────────────────────────────────────┘
```

**Three gRPC servers per pod:**
1. **Control** (kubelicate internal) — operator → PodRuntime
2. **Data** (kubelicate internal) — primary → secondary replication
3. **Client** (user-facing) — clients → KV store reads/writes

---

## Client gRPC API

```proto
service KvStore {
    rpc Get(GetRequest) returns (GetResponse);
    rpc Put(PutRequest) returns (PutResponse);
    rpc Delete(DeleteRequest) returns (DeleteResponse);
}
```

- **Put**: Primary only. Serializes `KvOp::Put{key,value}` → `replicate()` →
  applies to local HashMap → returns LSN.
- **Delete**: Primary only. Serializes `KvOp::Delete{key}` → `replicate()` →
  applies to local HashMap → returns LSN.
- **Get**: Reads from local HashMap. Checks `read_status()` — returns
  `UNAVAILABLE` if not `Granted`.

---

## Replicated Data Format

Operations are serialized as JSON for simplicity (a production app would
use protobuf or a binary format):

```rust
#[derive(Serialize, Deserialize)]
enum KvOp {
    Put { key: String, value: String },
    Delete { key: String },
}
```

Each `replicate()` call sends one `KvOp`. The LSN is assigned by the
replicator, not by the app.

---

## Shared State

```rust
struct KvState {
    data: HashMap<String, String>,
    last_applied_lsn: Lsn,
}
```

Wrapped in `Arc<RwLock<KvState>>` and shared between:
- The gRPC client server (reads + writes on primary)
- The lifecycle/state_provider event loop
- Copy/replication stream drain tasks on secondary

---

## Lifecycle Event Handling

### Open
Store handles (`partition`, `replicator`, `copy_stream`, `replication_stream`).
Initialize empty `KvState`.

### ChangeRole(IdleSecondary)
If `copy_stream` is available, spawn a task to drain it:
```
while let Some(op) = copy_stream.get_operation().await {
    apply KvOp to state
    op.acknowledge()
}
```

### ChangeRole(ActiveSecondary)
If `replication_stream` is available, spawn a task to drain it:
```
while let Some(op) = replication_stream.get_operation().await {
    apply KvOp to state
    op.acknowledge()
}
```
Reads are served (secondary read path) but writes are rejected.

### ChangeRole(Primary)
- Stop any stream drain tasks
- Start the client gRPC server (if not already running)
- Writes via `replicate()` are now allowed

### Close / Abort
Cancel background tasks, stop client gRPC server.

---

## StateProvider Callbacks

### GetLastCommittedLsn
Return `state.last_applied_lsn`. Called during build/catchup.

### GetCopyContext (secondary, during build)
Send a stream containing one operation: the secondary's current
`last_applied_lsn` as a string. This tells the primary what the
secondary already has.

### GetCopyState (primary, during build_replica)
1. Read peer LSN from copy_context stream
2. If peer has no state, produce full HashMap snapshot as operations
3. Each entry becomes a `KvOp::Put{key, value}`
4. Close the stream when done

### UpdateEpoch (secondary)
Receive new epoch + `previous_epoch_last_lsn`.
Rollback any operations above `previous_epoch_last_lsn` — for an
in-memory KV store without a WAL, this is a no-op (we only apply
committed ops from the stream, which are already at or below
committed_lsn).

### OnDataLoss
Accept state as-is (return `false` — state not changed).

---

## Data Flow: Primary Write

```
Client ──Put(k,v)──► KvServer
                        │
                        ▼
                  check write_status() == Granted
                        │
                        ▼
                  serialize KvOp::Put{k,v}
                        │
                        ▼
                  replicator.replicate(data, token)
                        │          (quorum replication)
                        ▼
                  apply to local HashMap
                  update last_applied_lsn = lsn
                        │
                        ▼
Client ◄──PutResponse{lsn}──
```

## Data Flow: Secondary Replication

```
Primary Replicator ──ReplicationItem──► Secondary Replicator
                                              │
                                              ▼
                                        queue → replication_stream
                                              │
                                              ▼
                                   user pulls: get_operation()
                                              │
                                              ▼
                                   apply KvOp to HashMap
                                   update last_applied_lsn
                                              │
                                              ▼
                                        op.acknowledge()
                                              │
                                              ▼
Secondary Replicator ──ACK──► Primary Replicator
                                    (quorum gate released)
```

---

## Demo Mode

Single-replica demo (no real replication):
1. Operator simulator: Open → Idle → Active → Primary
2. Primary writes a few KV pairs via gRPC client API
3. Reads them back
4. Demotes and closes

---

## File Structure

```
examples/kv-stateful/
├── Cargo.toml
├── build.rs
├── proto/
│   └── kvstore.proto            # Client API: Get/Put/Delete
└── src/
    ├── lib.rs                   # Module declarations
    ├── main.rs                  # Binary entry point + CLI args
    ├── state.rs                 # KvOp, KvState, SharedState, drain_stream
    ├── server.rs                # Client-facing KV gRPC server (KvServer)
    ├── service.rs               # Lifecycle + StateProvider event loop
    ├── demo.rs                  # Operator/client simulators for --demo mode
    ├── tests.rs                 # Operator-driven integration tests
    └── reconciler_tests.rs      # Reconciler-driven E2E tests
```

---

## Implementation Notes

- **Thread safety**: `KvState` is `Arc<RwLock<...>>`. Reads take a read lock,
  writes (from replicate or stream drain) take a write lock. The write lock is
  held only for the HashMap mutation, not during the `replicate()` await.

- **Client gRPC server lifecycle**: Started when promoted to Primary.
  Stopped on Close/Abort. Bind address is configurable via CLI args.

- **Serialization**: JSON via serde_json. Each `replicate()` payload is one
  `KvOp`. Copy state operations are also `KvOp::Put` for each entry.

- **Stream cancellation**: `drain_stream` accepts a `CancellationToken` and
  uses biased `select!` to stop cleanly on role change or shutdown.

- **Copy + replication fully wired**: BuildReplica on the primary runs the
  full data-plane copy protocol (GetCopyContext → GetCopyState → CopyStream).
  Replication stream delivers incremental ops with acknowledge-gated quorum.

---

## Tests (8)

**Operator-driven** (tests.rs):
- `test_operator_single_replica_kv` — single replica, Put/Get/Delete
- `test_operator_three_replica_failover` — 3 replicas, write, failover, write on new primary
- `test_operator_kv_crud_operations` — full CRUD: put, overwrite, delete, verify
- `test_operator_restart_secondary_copies_state` — restart secondary, verify copy stream delivers data
- `test_operator_scale_up` — scale 1→3, verify copy delivers all data to new replicas
- `test_operator_scale_down` — scale 3→1, verify primary still works

**Reconciler-driven** (reconciler_tests.rs):
- `test_reconciler_creates_partition_and_serves_kv` — full Pending→Creating→Healthy, write+read via KV API
- `test_reconciler_switchover` — switchover via target_primary, write on new primary, verify old rejects writes

---

## Persistent Storage Design

Currently `KvState` is in-memory only — all data is lost on pod restart,
requiring a full copy rebuild from the primary. This section designs a
simple file-based persistence layer with **zero new dependencies** (std
only).

### Goals

1. Survive pod restarts without full copy rebuild
2. Report correct `last_committed_lsn` on recovery (so the operator
   knows the replica is alive with recent state)
3. Support `UpdateEpoch` rollback (discard uncommitted ops — gap A6)
4. Zero new crate dependencies — use existing `tokio::fs`, `serde_json`,
   `tracing`, `bytes`
5. Always-persistent — no in-memory-only mode. All tests exercise the
   real persistence and recovery path.

### Non-Goals (For Now)

- High performance (we're an example app, not a database)
- Concurrent disk I/O
- Compaction or checkpointing (state is small)

### Storage Layout

```
/var/lib/kv-stateful/<partition-id>/
├── snapshot.json           # Full state checkpoint
├── wal/
│   ├── 000001.log          # Append-only operation log
│   ├── 000002.log          # New segment after checkpoint
│   └── ...
└── meta.json               # last_applied_lsn, epoch, segment info
```

For the simple case (small state), we can use a single-file approach:

```
/var/lib/kv-stateful/<partition-id>/
├── state.json              # Full HashMap + last_applied_lsn
└── wal.log                 # Append-only log of KvOps since last snapshot
```

### Approach: Snapshot + WAL

**Two files, two purposes:**

1. **`state.json`** — Periodic full snapshot of `KvState` (HashMap +
   last_applied_lsn). Written atomically via write-to-temp + rename.
   This is the checkpoint.

2. **`wal.log`** — Append-only log of `KvOp` entries since the last
   snapshot. Each line is one JSON entry with LSN. On recovery, load
   snapshot then replay WAL.

### WAL Entry Format

One JSON object per line (newline-delimited JSON / NDJSON):

```
{"lsn":1,"op":{"Put":{"key":"foo","value":"bar"}}}
{"lsn":2,"op":{"Delete":{"key":"baz"}}}
{"lsn":3,"op":{"Put":{"key":"qux","value":"123"}}}
```

Why NDJSON:
- Append-only: `writeln!` to an open file handle
- Crash-safe: partial last line is truncated on recovery
- Zero-dependency parsing: `serde_json::from_str` per line
- Human-readable for debugging

### WAL Entry Struct

```rust
#[derive(Serialize, Deserialize)]
struct WalEntry {
    lsn: Lsn,
    op: KvOp,
}
```

### Snapshot Format

```json
{
  "last_applied_lsn": 42,
  "data": {
    "key1": "value1",
    "key2": "value2"
  }
}
```

Written atomically: write to `state.json.tmp`, then `rename()` to
`state.json`. This guarantees the snapshot is always valid — a crash
during write leaves the old snapshot intact.

### KvState Changes

```rust
use std::path::PathBuf;
use tokio::fs;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, BufWriter};

pub struct KvState {
    pub data: HashMap<String, String>,
    pub last_applied_lsn: Lsn,
    /// Highest LSN confirmed committed by the replicator.
    /// Updated when the replicator advances committed_lsn.
    /// Checkpoints only capture state up to this LSN.
    pub committed_lsn: Lsn,
    wal_writer: BufWriter<tokio::fs::File>,
    data_dir: PathBuf,
}

impl KvState {
    /// Open with persistence. Loads snapshot + replays WAL.
    /// This is the only constructor — all code paths use persistent state.
    pub async fn open(data_dir: PathBuf) -> std::io::Result<Self> {
        fs::create_dir_all(&data_dir).await?;
        let mut state = Self::load_snapshot(&data_dir).await?;
        state.replay_wal(&data_dir).await?;
        // After replay, truncate WAL to remove any corrupt trailing
        // bytes from a previous crash. This prevents cascading data
        // loss: without truncation, new entries appended after the
        // corrupt line would be lost on next recovery (replay stops
        // at the first corrupt line).
        state.truncate_wal_to_valid(&data_dir).await?;
        let wal_writer = Self::open_wal_append(&data_dir).await?;
        Ok(Self {
            data: state.data,
            last_applied_lsn: state.last_applied_lsn,
            committed_lsn: state.last_applied_lsn, // assume snapshot is committed
            wal_writer,
            data_dir,
        })
    }

    /// Apply an operation and persist to WAL.
    /// Returns Err if WAL write fails — caller must NOT acknowledge
    /// the operation to the primary on error.
    pub async fn apply_op(&mut self, lsn: Lsn, op: &KvOp) -> std::io::Result<()> {
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
        // Append to WAL — propagate errors (do NOT swallow with let _)
        let entry = WalEntry { lsn, op: op.clone() };
        let line = serde_json::to_string(&entry)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
        self.wal_writer.write_all(line.as_bytes()).await?;
        self.wal_writer.write_all(b"\n").await?;
        // flush → OS page cache, then fdatasync → physical disk.
        // This ensures the entry is durable before acknowledge()
        // sends ACK to primary.
        self.wal_writer.flush().await?;
        self.wal_writer.get_ref().sync_data().await?;
        Ok(())
    }

    /// Update committed_lsn. Called when the replicator confirms
    /// committed_lsn has advanced (e.g., via configuration updates).
    /// Checkpoints are only safe up to this LSN.
    pub fn set_committed_lsn(&mut self, lsn: Lsn) {
        if lsn > self.committed_lsn {
            self.committed_lsn = lsn;
        }
    }
}
```

Note: `apply_op` returns `Result` — **WAL write failures must not be
swallowed.** If the WAL write fails, the caller must NOT call
`op.acknowledge()`. This prevents lying to the primary about durability.
The replicator will timeout waiting for the ACK and may rebuild the
replica.

**Durability levels:**
```
flush()                  → OS page cache (~0.01ms)
                           Survives: process crash
                           Lost on: power failure, kernel panic

flush() + sync_data()    → physical disk (~1-5ms on SSD)
                           Survives: process crash, power failure
                           This is fdatasync — syncs data only, not inode
```

We use `sync_data()` (fdatasync) because our WAL is append-only — the
inode metadata (size, mtime) doesn't need to be on disk for correctness.
`sync_all()` (fsync) would sync metadata too but is slower and
unnecessary for append-only files.

**Performance note:** Per-op fdatasync caps throughput at ~200-1000
ops/sec. This is acceptable for an example app. A production service
would use group commit (batch multiple ops into one fsync) — see the
WAL persistence design doc for details.

**No in-memory mode.** All code paths — including tests — use
`KvState::open(dir)`. Tests use `std::env::temp_dir()` with a unique
suffix per test. This ensures:
- Every test exercises the real persistence path
- Restart tests exercise real WAL recovery
- No divergence between test and production behavior

### Recovery Flow

```
KvState::open(data_dir).await:
  1. Load state.json via tokio::fs::read_to_string (or empty if missing)
     → data = HashMap, last_applied_lsn = N
  2. Open wal.log, read line by line via BufReader + lines()
     → For each WalEntry with lsn > last_applied_lsn:
       apply to HashMap directly (no WAL write during replay)
     → Skip entries with lsn <= last_applied_lsn (already in snapshot)
     → If last line is truncated (crash mid-write): stop replay
  3. Rewrite wal.log with only valid entries (truncate corrupt tail)
     → Prevents cascading data loss on next crash
  4. Open wal.log in append mode via OpenOptions + BufWriter
  5. Ready — state = snapshot + WAL replay
```

```rust
impl KvState {
    async fn load_snapshot(dir: &Path) -> std::io::Result<Self> {
        let path = dir.join("state.json");
        let (data, last_applied_lsn) = match fs::read_to_string(&path).await {
            Ok(contents) => {
                let snap: SnapshotData = serde_json::from_str(&contents)
                    .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
                info!(lsn = snap.last_applied_lsn, keys = snap.data.len(), "loaded snapshot");
                (snap.data, snap.last_applied_lsn)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                info!("no snapshot found, starting empty");
                (HashMap::new(), 0)
            }
            Err(e) => return Err(e),
        };
        // Return partial state — caller finishes initialization
        // (replay_wal, truncate, open writer)
        Ok(Self {
            data,
            last_applied_lsn,
            committed_lsn: last_applied_lsn, // snapshot is always committed
            wal_writer: BufWriter::new(fs::File::create(dir.join(".wal.tmp")).await?),
            data_dir: dir.to_path_buf(),
        })
    }

    async fn replay_wal(&mut self, dir: &Path) -> std::io::Result<()> {
        let path = dir.join("wal.log");
        let file = match fs::File::open(&path).await {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e),
        };
        let reader = BufReader::new(file);
        let mut lines = reader.lines();
        let mut replayed = 0u64;
        while let Ok(Some(line)) = lines.next_line().await {
            match serde_json::from_str::<WalEntry>(&line) {
                Ok(entry) if entry.lsn > self.last_applied_lsn => {
                    // Apply without WAL write (we're replaying the WAL itself)
                    match &entry.op {
                        KvOp::Put { key, value } => { self.data.insert(key.clone(), value.clone()); }
                        KvOp::Delete { key } => { self.data.remove(key); }
                    }
                    self.last_applied_lsn = entry.lsn;
                    replayed += 1;
                }
                Ok(_) => {} // Already in snapshot, skip
                Err(e) => {
                    warn!(error = %e, "truncated WAL entry, stopping replay");
                    break; // Crash mid-write — ignore partial line
                }
            }
        }
        info!(replayed, lsn = self.last_applied_lsn, "WAL replay complete");
        Ok(())
    }

    /// After replay, rewrite WAL with only valid entries to remove
    /// any corrupt trailing bytes. Without this, appending new entries
    /// after the corrupt line causes cascading data loss: next recovery
    /// stops at the corrupt line and loses everything after it.
    async fn truncate_wal_to_valid(&self, dir: &Path) -> std::io::Result<()> {
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

    async fn open_wal_append(dir: &Path) -> std::io::Result<BufWriter<fs::File>> {
        let file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join("wal.log"))
            .await?;
        Ok(BufWriter::new(file))
    }
}
```

### Snapshot (Checkpoint)

Take a snapshot periodically to bound WAL size. **Snapshots only
capture state up to `committed_lsn`** — never uncommitted ops. This
ensures `rollback_to()` can always discard uncommitted state by
reloading the snapshot.

```rust
impl KvState {
    /// Write a snapshot of committed state and truncate the WAL.
    ///
    /// SAFETY: Only snapshots committed state. Uncommitted ops
    /// (between committed_lsn and last_applied_lsn) remain in the
    /// WAL and can be discarded by rollback_to().
    pub async fn checkpoint(&mut self) -> std::io::Result<()> {
        // Build snapshot from committed state only.
        // For a KV store, we can't easily separate committed vs
        // uncommitted HashMap entries. Instead, we only checkpoint
        // when committed_lsn == last_applied_lsn (no uncommitted ops).
        if self.committed_lsn < self.last_applied_lsn {
            warn!(
                committed = self.committed_lsn,
                applied = self.last_applied_lsn,
                "skipping checkpoint: uncommitted ops present"
            );
            return Ok(());
        }

        let snapshot = SnapshotData {
            last_applied_lsn: self.committed_lsn,
            data: self.data.clone(),
        };

        // Atomic write: tmp → sync → rename
        let tmp = self.data_dir.join("state.json.tmp");
        let dst = self.data_dir.join("state.json");
        let json = serde_json::to_string(&snapshot)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
        fs::write(&tmp, &json).await?;
        // fsync the snapshot file before rename — ensures data is on
        // disk before the directory entry points to the new file.
        let tmp_file = fs::File::open(&tmp).await?;
        tmp_file.sync_data().await?;
        drop(tmp_file);
        fs::rename(&tmp, &dst).await?;

        info!(lsn = self.committed_lsn, keys = self.data.len(), "checkpoint written");

        // Truncate WAL (all entries up to committed_lsn are in snapshot)
        let wal_path = self.data_dir.join("wal.log");
        let file = fs::File::create(&wal_path).await?;
        self.wal_writer = BufWriter::new(file);
        Ok(())
    }
}
```

**Key invariant:** Snapshot LSN ≤ committed_lsn. This guarantees that
`rollback_to(previous_epoch_last_lsn)` can always discard uncommitted
ops — they live in the WAL, not the snapshot.

When to checkpoint:
- After copy stream completes (full state received — all committed)
- Periodically (every N ops, only when committed_lsn == last_applied_lsn)
- On graceful Close

### UpdateEpoch Rollback (A6 Fix)

When `UpdateEpoch(previous_epoch_last_lsn)` is called, the KV service
must discard any state beyond `previous_epoch_last_lsn`.

**IMPORTANT: Cancel drain tasks before rollback.** The `drain_stream`
tasks hold references to `SharedState` and call `apply_op()`. If
rollback runs while a drain is active, there's a race: the drain may
write to the WAL while rollback is rewriting it. This matches the
existing `ChangeRole` pattern — cancel `bg_token`, await handles,
then mutate state.

```rust
StateProviderEvent::UpdateEpoch { previous_epoch_last_lsn, reply, .. } => {
    // 1. Cancel active drain tasks BEFORE rollback
    if let Some(token) = bg_token.take() {
        token.cancel();
    }
    for h in bg_handles.drain(..) {
        let _ = h.await;
    }

    // 2. Rollback state if needed
    let mut guard = state.write().await;
    if guard.last_applied_lsn > previous_epoch_last_lsn {
        info!(
            current = guard.last_applied_lsn,
            rollback_to = previous_epoch_last_lsn,
            "rolling back uncommitted operations"
        );
        if let Err(e) = guard.rollback_to(previous_epoch_last_lsn).await {
            warn!(error = %e, "rollback failed");
        }
    }
    let _ = reply.send(Ok(()));

    // 3. Drains will be restarted on next ChangeRole
}
```

**Design note (A6 alignment):** This implements A6 Option 1 (user-side
rollback), which is appropriate for apps that can implement rollback
(KV stores can reload from snapshot). A6 Option 2 (defer dispatch until
committed) is the framework-level solution for apps that can't rollback
— that would be implemented in the replicator, not in the user service.

`rollback_to` implementation:
```rust
impl KvState {
    pub async fn rollback_to(&mut self, target_lsn: Lsn) -> std::io::Result<()> {
        let dir = self.data_dir.clone();
        // Reload snapshot (guaranteed to be at or below committed_lsn)
        let mut fresh = Self::load_snapshot(&dir).await?;
        // Replay WAL entries only up to target_lsn
        fresh.replay_wal_up_to(&dir, target_lsn).await?;
        // Replace current state
        self.data = fresh.data;
        self.last_applied_lsn = fresh.last_applied_lsn;
        // Rewrite WAL with only valid entries, then reopen for append
        self.rewrite_wal_up_to(target_lsn).await?;
        info!(lsn = self.last_applied_lsn, "rollback complete");
        Ok(())
    }

    async fn replay_wal_up_to(&mut self, dir: &Path, max_lsn: Lsn) -> std::io::Result<()> {
        let path = dir.join("wal.log");
        let file = match fs::File::open(&path).await {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e),
        };
        let mut lines = BufReader::new(file).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if let Ok(entry) = serde_json::from_str::<WalEntry>(&line) {
                if entry.lsn > self.last_applied_lsn && entry.lsn <= max_lsn {
                    match &entry.op {
                        KvOp::Put { key, value } => { self.data.insert(key.clone(), value.clone()); }
                        KvOp::Delete { key } => { self.data.remove(key); }
                    }
                    self.last_applied_lsn = entry.lsn;
                }
            }
        }
        Ok(())
    }

    async fn rewrite_wal_up_to(&mut self, max_lsn: Lsn) -> std::io::Result<()> {
        let wal_path = self.data_dir.join("wal.log");
        let tmp_path = self.data_dir.join("wal.log.tmp");

        // Read valid entries, write to tmp
        let mut entries = Vec::new();
        if let Ok(file) = fs::File::open(&wal_path).await {
            let mut lines = BufReader::new(file).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if let Ok(entry) = serde_json::from_str::<WalEntry>(&line) {
                    if entry.lsn <= max_lsn {
                        entries.push(line);
                    }
                }
            }
        }

        // Write filtered entries to tmp, sync, then rename
        let tmp_file = fs::File::create(&tmp_path).await?;
        let mut tmp = BufWriter::new(tmp_file);
        for line in &entries {
            tmp.write_all(line.as_bytes()).await?;
            tmp.write_all(b"\n").await?;
        }
        tmp.flush().await?;
        tmp.get_ref().sync_data().await?; // fsync before rename
        drop(tmp);
        fs::rename(&tmp_path, &wal_path).await?;

        // Reopen for append
        self.wal_writer = Self::open_wal_append(&self.data_dir).await?;
        Ok(())
    }
}
```

This is why the snapshot + WAL approach works well: the snapshot is
always at a known-committed LSN, and we can selectively replay the WAL
up to any point. Rollback = reload snapshot + replay WAL up to target.

### Integration with Service Lifecycle

```
Open:
  state = KvState::open(data_dir).await    ← Load snapshot + replay WAL
  GetLastCommittedLsn → state.last_applied_lsn  ← Correct on restart

ChangeRole(IdleSecondary):
  drain copy_stream → apply_op().await? → WAL appends
  If apply_op fails: do NOT acknowledge, log error, break drain
  state.checkpoint().await when copy completes  ← Snapshot after copy

ChangeRole(ActiveSecondary):
  drain replication_stream → apply_op().await? → WAL appends
  If apply_op fails: do NOT acknowledge (primary will timeout + rebuild)

ChangeRole(Primary):
  replicate() → apply_op().await? → WAL appends

UpdateEpoch:
  1. Cancel drain tasks (bg_token.cancel(), await handles)
  2. state.rollback_to(previous_epoch_last_lsn).await
  3. Drains restarted on next ChangeRole

Close:
  state.checkpoint().await  ← Snapshot for fast recovery
  drop wal_writer
```

**drain_stream error handling:**
```rust
pub async fn drain_stream(...) {
    loop {
        tokio::select! {
            biased;
            _ = token.cancelled() => break,
            item = stream.get_operation() => {
                let Some(op) = item else { break };
                let lsn = op.lsn;
                match serde_json::from_slice::<KvOp>(&op.data) {
                    Ok(kv_op) => {
                        // apply_op returns Result — if WAL write fails,
                        // do NOT acknowledge. The primary will timeout
                        // and may rebuild this replica.
                        match state.write().await.apply_op(lsn, &kv_op).await {
                            Ok(()) => op.acknowledge(),
                            Err(e) => {
                                warn!(lsn, error = %e, "WAL write failed, not acknowledging");
                                break; // Stop draining — replica is degraded
                            }
                        }
                    }
                    Err(e) => {
                        warn!(lsn, error = %e, "failed to deserialize op");
                    }
                }
            }
        }
    }
}
```

### File Structure Changes

```
examples/kv-stateful/src/
├── state.rs          # KvState + KvOp (add persistence methods)
├── persistence.rs    # NEW: WalEntry, load_snapshot, replay_wal,
│                     #       checkpoint, rollback_to
├── service.rs        # Event loop (add data_dir param, rollback)
├── server.rs         # KV gRPC server (unchanged)
├── testing.rs        # KvPod (add optional data_dir)
└── ...
```

### Test Impact

- **Always persistent** — no in-memory mode. Every test creates a temp
  dir and exercises the real persistence path. This ensures tests and
  production behavior never diverge.

- **Temp dir per test** — each test gets a unique directory under
  `std::env::temp_dir()`. No `tempfile` crate needed:
  ```rust
  let dir = std::env::temp_dir().join(format!("kv-test-{}-{}", id, std::process::id()));
  let state = Arc::new(RwLock::new(KvState::open(dir).await.unwrap()));
  ```

- **Restart tests** — reuse the same dir for real recovery:
  ```rust
  // Write data, then "crash"
  drop(state);
  // Restart — same dir, recovers from snapshot + WAL
  let state = Arc::new(RwLock::new(KvState::open(dir).await.unwrap()));
  assert_eq!(state.read().await.data.get("key"), Some(&"value".to_string()));
  ```

- **New persistence tests:**
  - Write data → "crash" → restart → verify data survives
  - Write data → checkpoint → more writes → "crash" → verify all data
  - Write data → UpdateEpoch rollback → verify uncommitted discarded
  - Copy stream → checkpoint → verify snapshot correct

`KvPod` creates a temp dir automatically:
```rust
impl KvPod {
    pub async fn start(id: i64) -> Self {
        let data_dir = std::env::temp_dir()
            .join(format!("kv-pod-{}-{}", id, std::process::id()));
        Self::start_with_dir(id, data_dir, Duration::from_secs(5)).await
    }

    /// Restart: crash + reopen with same data dir = real WAL recovery.
    pub async fn restart(self, id: i64) -> Self {
        let dir = self.data_dir.clone();
        self.crash().await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        Self::start_with_dir(id, dir, Duration::from_secs(5)).await
    }
}
```


---

### Implementation Status

| Feature | Status | Details |
|---------|--------|---------|
| `persistence.rs` module | Done | WAL/snapshot helpers: load, replay, truncate, checkpoint, rewrite |
| `KvState::open(dir)` | Done | Always-persistent constructor. Loads snapshot + replays WAL on startup |
| `apply_op()` async | Done | In-memory + WAL write + flush. Used by **all write paths** (server put/delete, drain_stream) |
| `apply_op_in_memory()` | Done | Sync in-memory apply. Used during WAL replay only |
| `checkpoint()` | Done | Atomic snapshot write (tmp + fsync + rename) + WAL truncate |
| Checkpoint on copy complete | Done | After IdleSecondary → ActiveSecondary, all copied state is checkpointed |
| Checkpoint on Close | Done | Graceful shutdown snapshots for fast recovery |
| `committed_lsn` tracking | Done | Prevents checkpointing uncommitted ops |
| WAL recovery on restart | Done | `truncate_wal_to_valid()` removes corrupt trailing bytes |
| UpdateEpoch rollback | Done | Rolls back uncommitted ops when `previous_epoch_last_lsn > 0 && < current` |
| Per-op WAL writes | Done | Both primary (server.rs) and secondary (drain_stream) persist to WAL before ACK |
| Unique temp dirs per test | Done | Atomic counter + pid ensures no cross-test pollution |
| `KvPod.data_dir` exposed | Done | Enables restart tests with same dir |
| All 36 tests pass | Done | 14 core + 22 kv-stateful, zero clippy warnings |

#### What's Deferred

| Feature | Reason | Notes |
|---------|--------|-------|
| **`KvPod::crash()` / `restart()`** | Not yet needed | Designed in testing.md. Now meaningful with per-op WAL writes |
| **Full A6 rollback** | Framework gap | `PartitionState::committed_lsn()` is never set on secondaries, so `previous_epoch_last_lsn` arrives as 0. Rollback is guarded (`> 0` check) to avoid wiping committed data. Needs framework fix: propagate committed_lsn to secondaries via the replication stream (B1 gap) |
| **B0: QuorumTracker timeout** | Pre-existing gap | If all secondaries die, `replicate()` hangs forever. Not related to WAL but exposed by timing changes in failover tests. See design-gaps.md |

#### Corrected Analysis: No RwLock Deadlock

The previous version of this document incorrectly diagnosed a deadlock
caused by `tokio::sync::RwLock` held across `.await` file I/O. This
analysis was wrong:

- `tokio::sync::RwLock` is **designed** to be held across `.await`
  points. When `drain_stream` holds `state.write().await` across WAL
  I/O (~1ms), the service event loop's `state.write().await` simply
  yields to the tokio runtime, the WAL I/O completes, the lock drops,
  and the event loop proceeds. This is temporary contention, not a
  deadlock — there is no circular dependency.

- The actual test hang was caused by **B0** (QuorumTracker has no
  timeout): after failover, the new primary's quorum configuration
  still includes the dead pod. Calls to `replicate()` wait for
  secondary ACKs that never arrive. Adding WAL I/O latency changed
  timing enough to expose this pre-existing issue.

The split-lock refactoring (separating WalWriter from KvState) was
unnecessary and has been reverted. Per-op WAL writes now work correctly
with the original `RwLock<KvState>` pattern.
