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
│   └── kvstore.proto
└── src/
    └── main.rs
```

---

## Implementation Notes

- **Thread safety**: `KvState` is `Arc<RwLock<...>>`. Reads take a read lock,
  writes (from replicate or stream drain) take a write lock. The write lock is
  held only for the HashMap mutation, not during the `replicate()` await.

- **Client gRPC server lifecycle**: Started when promoted to Primary or
  ActiveSecondary (for reads). Stopped on Close/Abort. Bind address is
  configurable via CLI args.

- **Serialization**: JSON via serde_json. Each `replicate()` payload is one
  `KvOp`. Copy state operations are also `KvOp::Put` for each entry.

- **MVP**: Copy and replication streams are not yet wired in kubelicate-core
  (build_replica is a no-op, replication_stream is None). The app code handles
  them correctly but they won't be exercised until core wiring is complete.
  The primary write path via `replicate()` works end-to-end.
