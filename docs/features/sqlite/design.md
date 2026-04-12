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

## Prior Art Review

### Approach 1: Statement-Level Replication (rqlite, dqlite)

**rqlite** (Go) and **dqlite** (C) replicate SQL statements via Raft
consensus. Each write SQL statement is appended to a Raft log, and
every node executes the same statements in the same order.

| Aspect | Detail |
|--------|--------|
| Replication unit | SQL statement text |
| Consensus | Raft (embedded) |
| Determinism | Required — `random()`, `now()` etc. cause divergence |
| Copy/rebuild | Raft snapshot (full SQLite DB file) |
| Read scaling | Stale reads on followers, linearizable reads via leader |

**Pros:** Simple, battle-tested. **Cons:** Non-deterministic SQL is
forbidden. Replaying 1M statements for rebuild is slow vs. shipping
a DB snapshot. Statement-level means schema must be identical everywhere.

### Approach 2: Page-Level Replication (LiteFS)

**LiteFS** (Go, by Fly.io) uses a FUSE filesystem to intercept SQLite
file I/O. On each committed transaction, it extracts the changed pages
and packages them into an **LTX file** (lite transaction) containing:
- Changed page data
- Rolling database checksum
- Sequential transaction ID (TXID)

LTX files are shipped via HTTP to replicas, which apply them in order.

| Aspect | Detail |
|--------|--------|
| Replication unit | Changed pages (LTX file per transaction) |
| Interception | FUSE filesystem (transparent to app) |
| Determinism | Not required — shipping pages, not statements |
| Copy/rebuild | Full DB snapshot + LTX replay from TXID |
| Consistency | Rolling checksum detects divergence |

**Pros:** Determinism-free, efficient incremental shipping, bitwise
replica consistency. **Cons:** FUSE dependency (Linux only), complex
infrastructure, HTTP-based (not quorum).

### Approach 3: VFS-Level Replication (mvSQLite, Verneuil)

**mvSQLite** replaces SQLite's VFS to store pages in FoundationDB.
**Verneuil** uses a VFS to replicate to S3. Both intercept at the
lowest level — every page read/write goes through custom code.

**Pros:** Full control, transparent to app. **Cons:** Massive
complexity, custom VFS is hard to get right (locking, shared memory,
crash recovery), tight coupling to storage backend.

### Approach 4: WAL-Level Replication (Litestream)

**Litestream** (Go) monitors the SQLite WAL file and ships WAL frames
to S3/SFTP for backup. It does NOT provide live replicas — it's a
continuous backup tool with point-in-time recovery.

**Pros:** Simple, non-invasive. **Cons:** Asynchronous only, no
quorum, no live read replicas, recovery requires full restore.

### Our Approach: WAL Frame Shipping via Kubelicate Quorum

We take the **LiteFS page-level concept** but use kubelicate's existing
quorum replication instead of HTTP, and intercept at the **application
layer** (WAL hook + file reads) instead of FUSE.

| Aspect | Our Design |
|--------|------------|
| Replication unit | WAL frames (committed transaction pages) |
| Interception | `sqlite3_wal_hook` + read WAL file directly |
| Consensus | Kubelicate quorum replication (not Raft) |
| Determinism | Not required — shipping pages, not statements |
| Copy/rebuild | Full DB file snapshot via GetCopyState |
| Failover | Kubelicate operator handles promotion |

**Why not statement-level:** Non-deterministic SQL restriction is
unacceptable for a general-purpose SQL database. Page-level replication
is strictly more correct.

**Why not custom VFS:** Massive implementation complexity for marginal
benefit. The WAL hook + file read approach gives us the same data with
much less code.

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
                                              │
                              ┌───────────────┘
                              ▼
                    committed_lsn advances
                              │
                              ▼
                    Apply committed frames to SQLite DB file
                    (direct page writes at correct offsets)
                              │
                              ▼
                    Read queries see new data
```

---

## SQLite WAL Internals (Relevant Subset)

SQLite WAL mode uses two files alongside the main database:
- **`<db>-wal`** — append-only log of page-sized frames, each with a
  24-byte header (page number, commit flag, salts, checksum) followed
  by the full page data
- **`<db>-shm`** — shared memory index mapping page numbers to WAL
  frame positions (rebuilt on open, not persisted)

**Key properties for replication:**
- A committed transaction writes one or more frames to the WAL, with
  the last frame's commit flag set (db_size field > 0)
- Each frame is self-describing: page number + page data
- Checkpointing merges WAL frames back into the main DB file
- WAL frames are idempotent: applying the same frame twice to a page
  produces the same result (last writer wins per page)

**WAL hook:** `sqlite3_wal_hook(db, callback, ctx)` fires after each
committed transaction with the number of frames in the WAL. This is
our interception point — we know frames were just appended.

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
checksums tied to the WAL file identity). Instead we extract the
logical content: which pages changed and what their new content is.
This is the same approach as LiteFS's LTX files.

### Primary: Capturing WAL Frames

On the primary, after each committed transaction:

1. **WAL hook fires** — tells us N frames were written
2. **Acquire exclusive WAL read lock** — prevents checkpoint from
   modifying the WAL while we read. Use `SQLITE_CHECKPOINT_PASSIVE`
   only after releasing this lock. Only one SQLite connection exists
   on the primary (no external connections allowed).
3. **Read the new frames** from the WAL file (we track our last-read
   position to know which frames are new). Skip if delta == 0 (empty
   transaction — `BEGIN; COMMIT;` with no writes).
4. **Package as `WalFrameSet`** — extract page numbers and page data,
   compute CRC32 checksum
5. **Release WAL read lock**
6. **Call `replicator.replicate(serialized_frame_set)`** — blocks until
   quorum ACK
7. **Return SQL result to client** only after quorum confirmation

**Timing trade-off:** The WAL hook fires AFTER SQLite has committed
the transaction locally. This means the primary has data that
secondaries may not yet have. If the primary crashes before
`replicate()` completes, this locally-committed data is lost on
failover (the new primary won't have it). This inverts the kvstore's
replicate-then-apply ordering. It is an inherent limitation of
using `sqlite3_wal_hook` — SQLite does not offer a pre-commit hook
that allows blocking on external I/O. See Known Problems.

**Checkpoint control:** The primary must NOT checkpoint the WAL while
frames haven't been replicated. We disable auto-checkpoint
(`sqlite3_wal_autocheckpoint(db, 0)`) and use a single SQLite
connection (no external tools or connections). Manual checkpoint
only runs after `committed_lsn` advances past the replicated
frames, and only while the WAL read lock is NOT held. This
serializes checkpoint and frame capture, preventing TOCTOU races.

### Secondary: Persist-then-ACK and Deferred Application

The secondary uses a **two-phase approach**: persist for durability
(before ACK), then apply to DB file (after commit confirmation).

**Prerequisites:** This model requires `committed_lsn` to be
propagated to secondaries. This is resolved — B5 fix adds
`committed_lsn` to `ReplicationItem`, piggybacked on every
replication message from primary to secondary.

**Phase 1 — Persist and ACK (on receive):**

1. **Deserialize** the `WalFrameSet`
2. **Verify CRC32 checksum** — reject corrupted frames (do not ACK)
3. **Append to `frames.log`** — length-prefixed binary format:
   `[4-byte length][payload][4-byte CRC32]` per entry. Length prefix
   enables recovery to skip corrupt trailing entries without scanning.
   Each entry is keyed by LSN.
4. **fsync** the frames.log file — ensures durability before ACK
5. **Acknowledge** — tells the primary "I durably have this data"

**Phase 2 — Apply to DB (on commit confirmation):**

When `committed_lsn` advances past a frame set's LSN:

1. **Read committed frames** from `frames.log`
2. **Apply pages to SQLite DB file** via `sqlite3_backup` API — this
   handles page cache invalidation, file locking, and database size
   changes correctly. The secondary opens a temporary in-memory DB
   with the new pages, then uses `sqlite3_backup_step()` to transfer
   them to the on-disk DB.
3. **Persist committed_lsn** to `meta.json` — needed for crash recovery
4. **GC frames.log** — remove applied entries periodically

Frames MUST be applied in LSN order. The replication stream delivers
them in order; the secondary must not reorder during application.

**Why two phases:** ACK to primary means "I durably have this data."
If we only buffered in memory, a secondary crash after ACK would lose
the data — the primary counted this ACK toward quorum but the data is
gone. Persisting to `frames.log` before ACK guarantees crash recovery.

Deferring DB application until commit means uncommitted pages never
touch the DB file, so rollback is trivial (truncate frames.log).

**Four files on secondary:**
```
<data-dir>/
├── <db>.sqlite          # Main DB file (only committed pages applied)
├── frames.log           # Length-prefixed binary log of received WalFrameSets
├── meta.json            # Persisted committed_lsn for crash recovery
└── (no SQLite WAL/SHM)  # SQLite WAL mode not used on secondary
```

### Copy Protocol (Full Snapshot)

For `GetCopyState` (new replica or full rebuild):

1. Primary pauses WAL hook processing (queue incoming writes)
2. Primary calls `sqlite3_backup_init()` to create a consistent snapshot
3. Records the current LSN as `copy_lsn` in `PartitionState::copy_lsn_map`
   — this is the boundary for replication queue replay (same as kvstore's
   C0 fix)
4. Streams the entire DB file as chunks via the copy stream
5. Resumes WAL hook processing
6. Secondary receives chunks, writes to local DB file
7. Secondary persists `committed_lsn = copy_lsn` to `meta.json`
8. Ready for incremental replication (actor replays from copy_lsn + 1)

**Concurrent writes during backup:** `sqlite3_backup_step()` restarts
if the source DB is modified. To prevent infinite restart under write
load, pause client writes during backup (reject with UNAVAILABLE).
This bounds backup duration to the time needed to copy the DB file.

---

## Client API

```proto
service SqliteStore {
    // Execute a write statement (INSERT/UPDATE/DELETE/CREATE/etc.)
    // Primary only. Returns affected rows and last insert rowid.
    rpc Execute(ExecuteRequest) returns (ExecuteResponse);

    // Execute a read query (SELECT).
    // Primary only. Returns rows as JSON or structured data.
    rpc Query(QueryRequest) returns (QueryResponse);

    // Execute multiple statements in a transaction.
    // Primary only.
    rpc ExecuteBatch(ExecuteBatchRequest) returns (ExecuteBatchResponse);
}
```

All client operations go through the primary. The client gRPC server
is only started on the primary (same as kvstore). Secondaries exist
solely for failover — they maintain a durable copy but don't serve
queries.

---

## Lifecycle Integration

The SQLite service follows the same two-channel pattern as kvstore:
- **`LifecycleEvent` channel** — Open, ChangeRole, Close, Abort
- **`StateProviderEvent` channel** — UpdateEpoch, GetCopyContext, GetCopyState, GetLastCommittedLsn, OnDataLoss

### LifecycleEvent Handling

| Event | Action |
|-------|--------|
| **Open** | Create data dir, open/create frames.log + meta.json. Return `ReplicatorHandle` |
| **ChangeRole(IdleSecondary)** | Drain copy stream — receive full DB snapshot, write to file |
| **ChangeRole(ActiveSecondary)** | Drain replication stream — persist frames to log, ACK, apply on commit |
| **ChangeRole(Primary)** | Open SQLite DB in WAL mode (single connection). Register WAL hook. Disable auto-checkpoint. Start client gRPC server |
| **Close** | Apply any remaining committed frames. Checkpoint SQLite WAL (primary). Persist committed_lsn. Close connections |
| **Abort** | Cancel drain tasks. Close file handles. No checkpoint |

### StateProviderEvent Handling

| Event | Action |
|-------|--------|
| **UpdateEpoch** | Truncate frames.log beyond `previous_epoch_last_lsn`. DB file is untouched (only committed pages applied). If `previous_epoch_last_lsn < persisted committed_lsn`, request full copy rebuild via `OnDataLoss` |
| **GetLastCommittedLsn** | Return persisted committed_lsn from meta.json |
| **GetCopyContext** | (Secondary) Send current committed_lsn as copy context |
| **GetCopyState** | (Primary) Pause WAL hook, snapshot DB via `sqlite3_backup`, record copy_lsn, stream chunks, resume WAL hook |
| **OnDataLoss** | Return `true` if state changed (data rolled back) |

### Checkpoint Strategy

**Primary:** Controls SQLite WAL checkpointing explicitly:
- After `committed_lsn` advances, checkpoint WAL frames up to that LSN
- Use `SQLITE_CHECKPOINT_PASSIVE` to avoid blocking readers
- Track checkpoint progress to know which WAL frames are safe to discard
- Checkpoint periodically or when WAL exceeds a size threshold

**Secondary:** GC `frames.log` after committed frames are applied to
the DB file. No SQLite WAL checkpointing needed (secondary doesn't use
SQLite's WAL mode).

---

## Key Design Decisions

### Why WAL Frame Shipping (not Statement Replication)

Statement-level replication (rqlite/dqlite approach) requires all SQL
to be deterministic. Functions like `random()`, `datetime('now')`,
`last_insert_rowid()` in triggers, and auto-increment behavior can
all cause replica divergence. WAL frame shipping eliminates this
entirely — we ship the result of execution, not the instructions.

### Why Application-Layer Interception (not VFS)

A custom VFS would intercept every file I/O operation and give us
complete control, but:
- SQLite's VFS API is complex (locking, shared memory, crash recovery)
- WAL mode VFS requires implementing the SHM (shared memory) interface
- Custom VFS bugs can corrupt the database silently
- The WAL hook + file read approach gives us the same data (committed
  frames) with ~100 lines of code vs. ~2000 for a VFS

We use rusqlite's `wal_hook` feature for notification, then read the
WAL file directly to extract frames.

### Why Not Ship Raw WAL Bytes

SQLite WAL files contain salt values and rolling checksums that are
specific to that WAL file instance. A new WAL file after checkpoint
has different salts. Shipping raw bytes would require the secondary
to maintain a compatible WAL file with matching salts — fragile and
unnecessary. Extracting logical page content (page_number + data)
is simpler and more robust.

### Page Application on Secondary

Pages are applied via `sqlite3_backup` API, not direct file writes.
Direct writes bypass SQLite's page cache, locking, and integrity
mechanisms — on promotion (secondary→primary), SQLite would open a
file it didn't write, with stale page cache and unknown lock state.
The backup API handles all of this correctly.

The backup API approach: open a temporary in-memory DB containing the
new pages, then `sqlite3_backup_step()` transfers them to the on-disk
DB. This ensures SQLite's internal state remains consistent and the
DB is safe to open as primary after promotion.

### Why Persist-then-ACK (Not Memory Buffer)

ACK to primary means "I durably have this data" — it counts toward
quorum. If the secondary only buffered in memory and crashed after
ACK, the data would be lost but the primary already counted the ACK.
This violates quorum safety: the primary thinks N replicas have the
data but fewer actually do.

Persisting to `frames.log` + fsync before ACK ensures that a crash
after ACK can always recover the data by replaying the log. This is
the same durability guarantee as the kvstore's per-op WAL writes.

---

## Rollback and Consistency

### UpdateEpoch Rollback

When a secondary receives `UpdateEpoch(previous_epoch_last_lsn)`, it
must discard any data beyond that LSN. Because we use the
**persist-then-ACK + deferred application** model, rollback is simple
for the common case:

1. **Truncate `frames.log`** — remove entries with LSN > target
2. **DB file is untouched** — uncommitted pages were never applied

**Edge case — rollback past committed data:** If `previous_epoch_last_lsn`
is less than the secondary's persisted `committed_lsn` (possible in
split-brain scenarios), pages from committed-then-rolled-back LSNs are
already baked into the DB file and cannot be un-applied. In this case,
the secondary must request a **full copy rebuild** from the new primary.
The service detects this condition and signals it via `OnDataLoss`.

**Recovery after crash:**
1. Read `committed_lsn` from `meta.json`
2. Open DB file (contains only committed data)
3. Replay `frames.log` — apply frames where `lsn <= committed_lsn`
   to DB file via backup API, discard frames where `lsn > committed_lsn`
4. Truncate corrupt trailing entries in frames.log (length-prefix
   framing enables this — read length, if payload + CRC is incomplete,
   truncate at that point)
5. Ready for incremental replication

**Primary rollback:** The primary uses SQLite's WAL normally. On
promotion after failover, the new primary opens the DB — SQLite's
WAL recovery handles any incomplete transactions automatically.

---

## File Structure

```
examples/sqlite/
├── Cargo.toml
├── build.rs
├── proto/sqlitestore.proto        # Client API: Execute/Query
└── src/
    ├── lib.rs
    ├── main.rs                    # Binary entry point + CLI args
    ├── state.rs                   # SqliteState, WAL frame capture/apply
    ├── server.rs                  # Client-facing SQL gRPC server
    ├── service.rs                 # Lifecycle + StateProvider event loop
    ├── frames.rs                  # WalFrameSet serialization, WAL reading
    ├── framelog.rs                # Secondary's frames.log: persist, replay, truncate
    ├── testing.rs                 # SqlitePod helper (feature = "testing")
    └── demo.rs                    # Optional demo mode
```

## Dependencies

```toml
[dependencies]
rusqlite = { version = "0.32", features = ["bundled", "hooks"] }
# ... existing kubelicate-core, tonic, serde, etc.
```

`bundled` links SQLite statically — no system dependency.
`hooks` enables `wal_hook`.

---

## Comparison with KVStore

| Aspect | KVStore | SQLite |
|--------|---------|--------|
| Data model | HashMap<String, String> | Full SQL (tables, indexes, etc.) |
| Replication unit | Serialized KvOp (Put/Delete) | WalFrameSet (page-level) |
| Determinism requirement | N/A (operations are deterministic) | None (page-level shipping) |
| State size | Small (all in memory) | Arbitrary (disk-backed) |
| Recovery | Snapshot + WAL replay | DB file snapshot + frame replay |
| Rollback | Reload snapshot + partial WAL | Truncate frames.log (deferred application) |
| Checkpoint | Atomic JSON snapshot | SQLite PASSIVE checkpoint |
| Secondary reads | No | No |
| Complexity | Low (~500 lines) | Medium (~1500 lines estimated) |

---

## Test Plan

Tests follow kvstore's pattern: in-process pods with real gRPC using
`SqlitePod` helper (analogous to `KvPod`). All tests use `#[serial]`
(port contention).

### Core Replication Tests

1. **test_single_write_replicates** — Primary: `CREATE TABLE + INSERT`.
   Verify secondary receives WalFrameSet and applies pages to DB file.
   Read secondary's DB directly (not via gRPC) to confirm data.

2. **test_multi_page_transaction** — Primary: single transaction that
   modifies many pages (bulk INSERT). Verify WalFrameSet contains
   multiple frames, all applied atomically on secondary.

3. **test_read_after_write** — Primary: `INSERT` then `SELECT` via
   client gRPC. Verify result matches. Basic round-trip.

4. **test_schema_changes_replicate** — Primary: `CREATE TABLE`,
   `ALTER TABLE ADD COLUMN`, `CREATE INDEX`. Verify secondary DB has
   identical schema (DDL is just page-level changes).

### Failover Tests

5. **test_failover_data_survives** — Write data on primary, trigger
   failover (remove primary from driver). New primary should serve
   the same data via `SELECT`.

6. **test_failover_uncommitted_lost** — Write data, crash primary
   before `replicate()` completes (simulate by not calling replicate).
   After failover, new primary should NOT have the uncommitted data.

### Copy Protocol Tests

7. **test_copy_full_rebuild** — Start 2-pod cluster, write data on
   primary. Add 3rd pod (idle secondary). Verify full DB snapshot
   transferred via GetCopyState and new secondary has all data.

8. **test_copy_then_incremental** — After copy completes, write more
   data on primary. Verify new secondary receives incremental
   WalFrameSets (not another full copy).

### Rollback / Epoch Tests

9. **test_update_epoch_truncates_frames_log** — Secondary has
   persisted but uncommitted frames. Send UpdateEpoch with
   `previous_epoch_last_lsn` below those frames. Verify frames.log
   truncated, DB file unaffected.

10. **test_update_epoch_past_committed_triggers_rebuild** — Secondary
    has committed and applied frames. Send UpdateEpoch with
    `previous_epoch_last_lsn` below committed_lsn. Verify service
    signals data loss (OnDataLoss returns true) and requests full copy.

### Crash Recovery Tests

11. **test_secondary_crash_recovery** — Secondary persists frames to
    frames.log, crash (kill process), restart. Verify committed_lsn
    recovered from meta.json, committed frames re-applied to DB,
    uncommitted frames discarded.

### Client API Tests

12. **test_execute_insert_update_delete** — Full CRUD via gRPC
    Execute/Query. Verify affected rows, last_insert_rowid.

13. **test_execute_batch_transaction** — Multiple statements in
    ExecuteBatch. Verify atomicity (all or nothing on error).

14. **test_query_returns_rows** — SELECT with multiple rows. Verify
    structured response format.

### Checkpoint Tests

15. **test_checkpoint_after_commit** — Write data, verify WAL grows.
    After committed_lsn advances, trigger checkpoint. Verify WAL
    shrinks and data persists in main DB file.

---

## Open Questions

1. **Page size:** Default 4096. Should we support configurable page sizes
   or mandate 4096? Larger pages (8K, 16K) reduce frame count but
   increase replication payload per frame.

2. **Batch vs. per-transaction replication:** Ship one `WalFrameSet` per
   SQL transaction, or batch multiple small transactions? Per-transaction
   is simpler and matches the WAL hook granularity.

3. **Schema migrations:** DDL (CREATE TABLE, ALTER) is just SQL — shipped
   as page changes like any other write. No special handling needed.

4. **Large transactions:** A transaction that modifies 10,000 pages
   creates a large `WalFrameSet` (~40MB at 4K pages). Tonic's default
   gRPC message limit is 4MB. Mitigations: configure
   `max_encoding_message_size` / `max_decoding_message_size` on both
   client and server, or chunk large frame sets into multiple
   `replicate()` calls with a continuation flag.

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
