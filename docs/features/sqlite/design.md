# SQLite: Replicated SQLite on Kubelicate

A replicated SQLite database running on kubelicate-core. The primary
accepts SQL reads and writes, ships WAL frames to secondaries via the
replication protocol. Secondaries exist for failover — they maintain a
durable copy of the database but do not serve client queries.

---

## Goals

1. Full SQL interface — clients issue arbitrary SQL (reads + writes)
2. Primary ships SQLite WAL frames to secondaries via kubelicate quorum
3. Secondaries maintain a durable copy for failover (no client reads)
4. Survive pod restarts — secondary catches up via copy (full DB snapshot)
   or WAL replay (incremental)
5. Leverage existing kubelicate-core lifecycle, replication, and failover
6. Zero consensus layer — kubelicate's quorum replication IS the consensus

## Non-Goals

- Multi-writer / multi-primary (single primary, same as kvstore)
- Sharding across partitions (single partition = single SQLite DB)
- Custom VFS implementation (use standard SQLite WAL mode, intercept
  at the application layer)
- Sub-millisecond replication latency (WAL frames are shipped per-commit)

---

## Prior Art

| Approach | Examples | Replication Unit | Determinism | Trade-off |
|----------|----------|-----------------|-------------|-----------|
| Statement-level | rqlite, dqlite | SQL text via Raft | Required | Simple but forbids non-deterministic SQL |
| Page-level | LiteFS | Changed pages (LTX) | Not required | Correct but FUSE dependency |
| VFS-level | mvSQLite, Verneuil | Pages via custom VFS | Not required | Full control but massive complexity |
| WAL backup | Litestream | WAL frames to S3 | N/A | Simple but async-only, no live replicas |

### Our Approach: WAL Frame Shipping via Kubelicate Quorum

We take the **LiteFS page-level concept** but use kubelicate's existing
quorum replication instead of HTTP, and intercept at the **application
layer** (WAL file reads after commit) instead of FUSE.

| Aspect | Our Design |
|--------|------------|
| Replication unit | WAL frames (committed transaction pages) |
| Interception | Read WAL file after commit |
| Consensus | Kubelicate quorum replication (not Raft) |
| Determinism | Not required — shipping pages, not statements |
| Copy/rebuild | Full DB file snapshot via GetCopyState |
| Failover | Kubelicate operator handles promotion |

---

## Architecture

```
Clients (SQL over gRPC)
    │
    ▼
┌──────────────────────────────────────────────────────────┐
│                  sqlite Pod                            │
│                                                           │
│  ┌──────────────┐    ┌──────────────────────────────┐    │
│  │  PodRuntime   │    │  SQLite Service (user app)    │    │
│  │               │    │                              │    │
│  │ control gRPC ◄┤    │  ┌────────────────────────┐  │    │
│  │ data gRPC    ◄┤    │  │  SQLite DB (WAL mode)  │  │    │
│  │               │    │  │  + WAL hook callback    │  │    │
│  │ lifecycle_tx ─┼───►│  └────────────────────────┘  │    │
│  │ state_prov_tx─┼───►│                              │    │
│  └──────────────┘    │  client gRPC ◄── Clients     │    │
│                       └──────────────────────────────┘    │
└──────────────────────────────────────────────────────────┘
```

### Primary Flow

```
Client ──SQL write──► SqliteServer
                         │
                         ▼
                   BEGIN (implicit or explicit)
                   Execute SQL statements
                   COMMIT
                         │
                         ▼
                   WAL hook fires:
                     read new WAL frames from file
                     serialize as WalFrameSet { pages, db_size }
                         │
                         ▼
                   replicator.replicate(WalFrameSet)
                     (quorum — blocks until ACKed)
                         │
                         ▼
Client ◄──Result──
```

### Secondary Flow

```
Primary Replicator ──ReplicationItem──► Secondary Replicator
                                              │
                                              ▼
                                    drain_stream receives WalFrameSet
                                              │
                                              ▼
                                    Persist to local frames.log + fsync
                                              │
                                              ▼
                                         acknowledge()

On promotion (ChangeRole Primary):
  Read frames.log → apply pages to DB file → open SQLite
```

---

## Replication Protocol

### Replication Unit: WalFrameSet

```rust
#[derive(Serialize, Deserialize)]
struct WalFrameSet {
    /// The WAL frames from this transaction.
    /// Each frame is (page_number, page_data).
    frames: Vec<WalFrame>,
    /// Database size in pages after this transaction
    /// (from the WAL commit frame's header).
    db_size_pages: u32,
    /// CRC32 checksum over all frame data for integrity verification.
    /// Computed by primary, verified by secondary before persisting.
    checksum: u32,
}

#[derive(Serialize, Deserialize)]
struct WalFrame {
    page_number: u32,
    data: Vec<u8>,  // page_size bytes (typically 4096)
}
```

**Serialization:** WalFrameSet is serialized via `serde_json` (matching
kvstore's approach). The replication channel transmits `Bytes` — the
service serializes with `serde_json::to_vec()` and deserializes on
receive. This is simple and debuggable; binary formats can be adopted
later if throughput requires it.

We do NOT ship raw WAL bytes (which include SQLite-specific salts and
checksums tied to the WAL file identity). We extract the logical
content: which pages changed and what their new content is.

### Primary: Capturing WAL Frames

After each SQL write, the primary reads new frames from the WAL file
(tracking the last-read offset), packages them as `WalFrameSet` with
CRC32 checksum, and calls `replicator.replicate()` which blocks until
quorum ACK. Auto-checkpoint is disabled (`wal_autocheckpoint=0`);
single connection enforced.

**Timing:** WAL frames are read AFTER SQLite commits locally. See
Known Problems (KP-1) for the timing inversion trade-off.

### Secondary: Persist-then-ACK, Deferred Apply

The secondary persists frames for durability before ACK. Application
to the DB file is **deferred until promotion**.

**On receive:** Deserialize → verify CRC32 → append to `frames.log`
(length-prefixed: `[4B len][JSON payload][4B CRC32]`) → fsync → ACK.

**On promotion:** Read all frames from `frames.log` → apply pages to
DB file via direct writes at `(page_number - 1) * page_size` → open
SQLite in WAL mode with a fresh connection.

This keeps the ACK path fast (one file write + fsync, no DB page I/O).
Rollback is trivial (truncate frames.log — pages never touch DB file).

**Secondary data directory:**
```
<data-dir>/
├── db.sqlite            # DB file (pages applied on promotion only)
├── frames.log           # Length-prefixed binary log of WalFrameSets
├── meta.json            # Persisted committed_lsn for crash recovery
└── (no SQLite WAL/SHM)  # WAL mode not used on secondary
```

### Copy Protocol (Full Snapshot)

For `GetCopyState` (new replica or full rebuild):

1. Primary checkpoints WAL (`PRAGMA wal_checkpoint(PASSIVE)`)
2. Reads the entire DB file as a byte array
3. Records current LSN as `copy_lsn` in `PartitionState::copy_lsn_map`
4. Streams as a single Operation via the copy stream
5. Secondary writes to local `db.sqlite`, sets `committed_lsn = copy_lsn`
6. Ready for incremental replication (actor replays from `copy_lsn + 1`)

---

## Client API

gRPC service `SqliteStore` with three RPCs (primary only):
- **Execute** — write statements (INSERT/UPDATE/DELETE/DDL). Returns `rows_affected`, `last_insert_rowid`, `lsn`.
- **Query** — read queries (SELECT). Returns typed columns and rows.
- **ExecuteBatch** — multiple statements in a transaction. Returns per-statement `rows_affected`.

See `proto/sqlitestore.proto` for full message definitions.

---

## Lifecycle Integration

Same two-channel pattern as kvstore (`LifecycleEvent` + `StateProviderEvent`).

| Event | Action |
|-------|--------|
| **Open** | Create data dir, load meta.json. Return `ReplicatorHandle` |
| **ChangeRole(Idle)** | Open frame log, drain copy stream (full DB snapshot) |
| **ChangeRole(Active)** | Drain replication stream — persist frames, ACK |
| **ChangeRole(Primary)** | Apply frames from log to DB, open SQLite (WAL mode, auto-checkpoint off), start gRPC server |
| **Close** | Checkpoint WAL (primary), close connections |
| **Abort** | Cancel drains, close |
| **UpdateEpoch** | Truncate frames.log beyond `previous_epoch_last_lsn` |
| **GetCopyState** | Checkpoint WAL, send full DB file as snapshot |
| **GetLastCommittedLsn** | Return `committed_lsn` from meta.json |

---

## Key Design Decisions

- **WAL frame shipping (not statement replication):** Eliminates
  determinism requirement. `random()`, `datetime('now')`, triggers all
  work — we ship the result of execution, not the instructions.
- **Application-layer interception (not VFS):** WAL file read after
  commit gives us the same data as a custom VFS with ~100 lines vs
  ~2000. Uses rusqlite's WAL hook for notification.
- **Logical page content (not raw WAL bytes):** WAL files contain
  file-specific salts and checksums. We extract `(page_number, data)`
  pairs — simpler and portable across WAL file instances.
- **Direct page writes on secondary (not sqlite3_backup):** Secondary
  doesn't have SQLite open, so no page cache to invalidate. On
  promotion, SQLite opens the file fresh with a clean page cache.
- **Persist-then-ACK (not memory buffer):** ACK counts toward quorum.
  `frames.log + fsync` before ACK ensures crash recovery.
- **Deferred apply (not per-receive):** Pages applied only on promotion.
  Keeps ACK path fast, rollback trivial (truncate frames.log).
- **tokio::sync::Mutex (not RwLock):** rusqlite `Connection` is `Send`
  but not `Sync`. Mutex only requires `Send`; RwLock requires both.

---

## Rollback and Consistency

**UpdateEpoch rollback:** Truncate `frames.log` beyond target LSN.
DB file is untouched (deferred apply means uncommitted pages were
never applied).

**Rollback past committed data:** If `previous_epoch_last_lsn <
committed_lsn`, applied pages can't be un-applied. Service detects
this and signals `OnDataLoss` for full copy rebuild.

**Crash recovery:** Read `committed_lsn` from `meta.json`, replay
`frames.log` up to that LSN, truncate corrupt trailing entries.

---

## File Structure

```
examples/sqlite/
├── Cargo.toml
├── build.rs
├── proto/sqlitestore.proto
├── src/
│   ├── lib.rs, main.rs, demo.rs
│   ├── state.rs         # SqliteState: WAL capture (primary), page apply (secondary)
│   ├── server.rs        # Client gRPC server (Execute/Query/ExecuteBatch)
│   ├── service.rs       # Lifecycle + StateProvider event loop
│   ├── frames.rs        # WalFrameSet/WalFrame types, WAL file parser
│   ├── framelog.rs      # frames.log + meta.json persistence
│   └── testing.rs       # SqlitePod helper (feature = "testing")
└── tests/
    ├── operator_basic.rs       # CRUD, read-after-write, query, batch
    ├── operator_replication.rs  # Single write, multi-page, schema DDL
    └── operator_failover.rs    # Failover + switchover data survival
```

Key dependency: `rusqlite = { features = ["bundled", "hooks"] }` —
statically links SQLite, enables WAL hook. Also `crc32fast` for checksums.

---

## Comparison with KVStore

| Aspect | KVStore | SQLite |
|--------|---------|--------|
| Data model | HashMap<String, String> | Full SQL |
| Replication unit | Serialized KvOp | WalFrameSet (page-level) |
| Determinism | N/A | Not required |
| State | All in memory | Disk-backed |
| Secondary ACK | Apply to HashMap + WAL | Persist to frames.log only |
| Promotion apply | N/A (already in memory) | Replay frames.log → DB file |
| Rollback | Reload snapshot + partial WAL | Truncate frames.log |
| Checkpoint | Atomic JSON snapshot | `PRAGMA wal_checkpoint(PASSIVE)` |

---

## Test Plan

Tests use in-process pods with real gRPC via `SqlitePod` helper.
All tests use `#[serial]` (port contention).

### Implemented (Phase 1)

| # | Test | File |
|---|------|------|
| 1 | test_single_write_replicates | operator_replication.rs |
| 2 | test_multi_page_transaction | operator_replication.rs |
| 3 | test_read_after_write | operator_basic.rs |
| 4 | test_schema_changes_replicate | operator_replication.rs |
| 5 | test_failover_data_survives | operator_failover.rs |
| 12 | test_execute_insert_update_delete | operator_basic.rs |
| 13 | test_execute_batch_transaction | operator_basic.rs |
| 14 | test_query_returns_rows | operator_basic.rs |
| — | test_switchover_data_survives | operator_failover.rs |

### Planned (Phase 2+)

| # | Test | Category |
|---|------|----------|
| 6 | test_failover_uncommitted_lost | Failover |
| 7 | test_copy_full_rebuild | Copy protocol |
| 8 | test_copy_then_incremental | Copy protocol |
| 9 | test_update_epoch_truncates_frames_log | Rollback |
| 10 | test_update_epoch_past_committed_triggers_rebuild | Rollback |
| 11 | test_secondary_crash_recovery | Crash recovery |
| 15 | test_checkpoint_after_commit | Checkpoint |

---

## Open Questions

1. **Large transactions:** A transaction modifying 10,000 pages creates
   a ~40MB `WalFrameSet`. Tonic's default gRPC limit is 4MB. May need
   `max_encoding_message_size` / `max_decoding_message_size` config.

2. **Fault reporting on promotion failure:** If `open_as_primary` fails
   (disk full, permissions), the service currently logs a warning but
   continues — the client gRPC server starts with no connection and all
   queries panic. Should report fault via `fault_tx` to trigger operator
   failover/rebuild. Same applies to `apply_committed_frames` failure.

---

## Known Problems

These are inherent limitations that cannot be fully resolved in this
design. They are documented for awareness and future mitigation.

### KP-1: WAL Hook Timing Inversion

`sqlite3_wal_hook` fires AFTER SQLite has committed the transaction
locally. This means `replicate()` runs after local commit, inverting
the kvstore's replicate-then-apply ordering.

**Impact:** If the primary crashes between local commit and quorum
confirmation, locally-committed data is lost on failover. From the
client's perspective the write failed (no response received), but the
primary's local DB has the data.

**Why not fixable:** SQLite has no pre-commit hook that allows blocking
on external I/O. `sqlite3_commit_hook` can abort a transaction but
cannot pause it while waiting for network I/O — it must return
synchronously. The WAL hook is the only point where we know frames
are durably written.

**Mitigation:** This is the same trade-off LiteFS makes. The window
is small (sub-millisecond on local commit to start of replicate).
Client retry on timeout is safe because page-level replication is
idempotent. On failover, the new primary has a consistent state —
it just may be missing the last transaction from the old primary.
This is **not corruption** — the new primary holds a valid prefix of
the old primary's state, and the client never received a success
response for the lost transaction.

**VFS alternative:** A custom VFS could fix this by intercepting
`xWrite()` calls to the WAL file, shipping pages to the replicator
*before* letting SQLite flush locally (replicate-then-commit). This
eliminates the timing window entirely. However, implementing a WAL-
mode VFS requires ~2000 lines (vs ~100 for WAL file reads), including
`xShmMap`/`xShmLock`/`xShmBarrier` for shared memory, and VFS bugs
can silently corrupt the database. The current approach is the right
trade-off for an example app; a production system would consider VFS
or a purpose-built replication engine (e.g., dqlite).

### KP-2: Per-Commit Synchronous Replication Throughput

Every SQL COMMIT pays a full network RTT for quorum replication.
With page-level payloads (~16KB per simple INSERT vs. ~100 bytes
in KV), throughput is limited to ~500-1000 TPS at 1ms RTT.

**Mitigation:** Acceptable for an example app. Production systems
would use group commit (batch multiple transactions' frames into
one `replicate()` call) — this is a future optimization, not a
design change.

---

## References

- [SQLite WAL Mode](https://sqlite.org/wal.html)
- [SQLite WAL File Format](https://sqlite.org/walformat.html)
- [LiteFS Architecture](https://github.com/superfly/litefs)
- [LiteFS WAL Mode Blog](https://fly.io/blog/wal-mode-in-litefs/)
- [rqlite Design](https://rqlite.io/docs/design/)
- [dqlite Replication](https://canonical.com/dqlite/docs/explanation/replication)
- [Litestream How It Works](https://litestream.io/how-it-works/)
- [mvSQLite Design](https://su3.io/posts/mvsqlite)
- [rusqlite WAL Hook](https://docs.rs/rusqlite/latest/rusqlite/hooks/struct.Wal.html)
