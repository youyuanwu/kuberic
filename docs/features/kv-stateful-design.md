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
replicator actor (monotonic counter in its single `select!` loop) and
returned to the caller. Users never provide LSNs — this is a deliberate
design choice matching SF's `IReplicator::Replicate()`:

- **Total ordering:** The actor serializes LSN assignment, guaranteeing
  monotonic, gap-free sequences. User-generated LSNs could arrive out of
  order from concurrent callers, breaking QuorumTracker, ReplicationQueue,
  and secondary WAL replay which all assume monotonic LSNs.
- **No gaps or duplicates:** The counter guarantees contiguity. Users
  could produce gaps (1, 2, 5) or duplicates (1, 2, 2), corrupting
  replication queue lookups and quorum tracking.
- **Epoch fencing:** The replicator gates LSN assignment on primary role.
  User-generated LSNs would bypass this — a stale primary could generate
  conflicting LSNs during a network partition.

LSN generation is a core replicator responsibility. Moving it to the
user would require the user to solve distributed total-order broadcast
— which is the problem the replicator exists to solve.

---

## Shared State

```rust
struct KvState {
    data: HashMap<String, String>,
    last_applied_lsn: Lsn,
    committed_lsn: Lsn,
    wal_writer: BufWriter<tokio::fs::File>,
    data_dir: PathBuf,
}
```

Wrapped in `Arc<RwLock<KvState>>` and shared between:
- The gRPC client server (reads + writes on primary)
- The lifecycle/state_provider event loop
- Copy/replication stream drain tasks on secondary

---

## Lifecycle Event Handling

| Event | Action |
|-------|--------|
| **Open** | `KvState::open(data_dir)` — load snapshot + replay WAL |
| **ChangeRole(IdleSecondary)** | Spawn `drain_stream` for `copy_stream` — applies ops via `apply_op()` (WAL + in-memory) |
| **ChangeRole(ActiveSecondary)** | Wait for copy drain to finish naturally, checkpoint, then spawn `drain_stream` for `replication_stream` |
| **ChangeRole(Primary)** | Start client gRPC server |
| **Close** | Cancel drains, stop client server, checkpoint for fast recovery |
| **Abort** | Cancel drains, stop client server (no checkpoint) |

## StateProvider Callbacks

| Callback | Behavior |
|----------|----------|
| **GetLastCommittedLsn** | Return `state.last_applied_lsn` |
| **GetCopyContext** | Send secondary's `last_applied_lsn` as copy context |
| **GetCopyState** | Read peer LSN, produce full HashMap as `KvOp::Put` ops. Drops read lock before sending to avoid blocking drains |
| **UpdateEpoch** | If `previous_epoch_last_lsn > 0` and `< current_lsn`: cancel drains, `rollback_to(previous_epoch_last_lsn)`. See [A6 gap](kubelicate/design-gaps.md) |
| **OnDataLoss** | Accept state as-is (return `false`) |

---

## Data Flow

**Primary write:**
Client → KvServer → check `write_status()` → serialize `KvOp` →
`replicator.replicate()` (quorum) → `apply_op()` (in-memory + WAL) →
return LSN.

**Secondary replication:**
Primary replicator → gRPC ReplicationItem → SecondaryReceiver →
OperationStream → `drain_stream` → `apply_op()` (in-memory + WAL) →
`acknowledge()` → ACK back to primary → quorum gate released.

---

## File Structure

```
examples/kv-stateful/
├── Cargo.toml
├── build.rs
├── proto/kvstore.proto           # Client API: Get/Put/Delete
├── src/
│   ├── lib.rs                    # Module declarations
│   ├── main.rs                   # Binary entry point + CLI args (--data-dir)
│   ├── state.rs                  # KvOp, KvState, SharedState, drain_stream
│   ├── persistence.rs            # WAL/snapshot helpers: load, replay, checkpoint, rollback
│   ├── server.rs                 # Client-facing KV gRPC server (KvServer)
│   ├── service.rs                # Lifecycle + StateProvider event loop
│   ├── testing.rs                # KvPod helper (feature = "testing")
│   └── demo.rs                   # Operator/client simulators for --demo mode
└── tests/                        # Integration tests (operator_basic, failover, replication, reconciler)
```

## Implementation Notes

- **Thread safety**: `KvState` is `Arc<RwLock<...>>`. `tokio::sync::RwLock`
  is designed to be held across `.await`. The write lock is held across
  WAL I/O (~1ms) — this is temporary contention, not a deadlock.

- **Client gRPC server lifecycle**: Started on Primary promotion, stopped
  on Close/Abort. Bind address configurable via CLI.

- **Serialization**: JSON via serde_json. Each `replicate()` payload is
  one `KvOp`. Copy state uses `KvOp::Put` for each HashMap entry.

- **Stream cancellation**: `drain_stream` uses `CancellationToken` with
  biased `select!`. Copy drain finishes naturally on IdleSecondary →
  ActiveSecondary (no cancellation — prevents item loss).

---

## Persistent Storage

KvState persists to disk via snapshot + WAL. Every code path — including
tests — uses `KvState::open(dir)`. No in-memory-only mode.

### Storage Layout

```
<data-dir>/
├── state.json              # Full HashMap + last_applied_lsn (atomic checkpoint)
└── wal.log                 # Append-only NDJSON log of KvOps since last snapshot
```

### Design Decisions

**Snapshot + WAL approach:** Two files, two purposes. `state.json` is a
periodic full snapshot written atomically (write-to-tmp + fsync + rename).
`wal.log` is an append-only NDJSON log. Recovery = load snapshot + replay
WAL. This enables rollback: snapshot is always at a committed LSN, WAL
entries can be selectively replayed up to any target LSN.

**Per-op WAL writes on quorum path:** Both primary (`server.rs` put/delete)
and secondary (`drain_stream`) call `apply_op()` which writes to the WAL
and flushes before acknowledging. WAL failure prevents ACK, so the primary
will timeout and rebuild the replica. This is the correct behavior — never
lie about durability.

**Durability:** `flush()` to OS page cache (survives process crash).
`sync_data()` (fdatasync) to disk is available but commented out — per-op
fdatasync caps throughput at ~200-1000 ops/sec, acceptable for an example
app but not production. A production service would use group commit.

**No in-memory mode:** All tests use real persistence via unique temp dirs
(atomic counter + pid). This ensures test and production behavior never
diverge, and enables future restart/recovery tests.

### Recovery

```
KvState::open(data_dir):
  1. Load state.json (or empty if missing)
  2. Replay wal.log entries with lsn > snapshot LSN
  3. Truncate corrupt trailing WAL bytes (crash-safe)
  4. Reopen WAL for append
```

### Checkpoint

Snapshots only when `committed_lsn == last_applied_lsn` (no uncommitted
ops). Triggered on:
- Copy stream completion (IdleSecondary → ActiveSecondary)
- Graceful Close

### UpdateEpoch Rollback

When `UpdateEpoch(previous_epoch_last_lsn)` arrives, the service:
1. Cancels active drain tasks (prevents races)
2. Calls `rollback_to(previous_epoch_last_lsn)` — reloads snapshot +
   partial WAL replay + WAL rewrite

**UpdateEpoch rollback (A6 + B5):** With B5 fix, `committed_lsn` is
propagated to secondaries via `ReplicationItem`. `previous_epoch_last_lsn`
is now correct. Rollback fires when `previous_epoch_last_lsn < current_lsn`,
reloading from snapshot + partial WAL replay. Drain tasks are NOT cancelled
during rollback — the replication stream continues (matches SF).

### Remaining Work

| Feature | Status | Notes |
|---------|--------|-------|
| `KvPod::crash()` / `restart()` | Designed | In testing.md. Now meaningful with per-op WAL writes |
| B0: QuorumTracker timeout | Pre-existing gap | `replicate()` hangs forever if all secondaries die |
