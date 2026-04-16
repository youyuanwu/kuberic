# Kuberic: Writer-Based Replication

> **Status: ABANDONED.** This design separates the Writer from the
> replicas to eliminate the primary intermediary (1 hop instead of 2).
> However, separating the Writer creates a fundamental correctness
> problem: `committed_lsn` (which ops are quorum-committed) is known
> only to the Writer. If the Writer crashes, this information is lost,
> and replicas may roll back committed ops — data loss.
>
> The fix requires the Writer to externalize `committed_lsn` before
> returning success to the client. Both options are unsatisfying:
>
> - **Fire-and-forget** (send `committed_lsn` to coordinator without
>   waiting for ACK): The Writer can crash after returning `Ok` to the
>   client but before the message reaches the coordinator (TCP buffer
>   not flushed). Microsecond window, but a real **correctness hole** —
>   a committed op can be rolled back.
>
> - **Strict** (wait for coordinator to persist `committed_lsn` to
>   etcd before returning `Ok`): Adds coordinator RTT + etcd write
>   latency (~1-5ms) to every write. Total latency is comparable to
>   or **worse** than the existing 2-hop leader-based system, defeating
>   the design's latency motivation.
>
> The root cause: the existing leader-based system gets correctness
> for free because the primary IS a replica — its local state is
> authoritative, no external `committed_lsn` tracking needed. Removing
> the primary saves 1 hop but loses this property. Recovering it costs
> exactly what was saved — either a correctness window or an extra hop.
>
> The existing leader-based operator (`kuberic-operator`) remains the
> optimal design for write latency with correct semantics.
>
> The analysis below is preserved for reference.

---

Design for a **new** operator and control system that decouples the
quorum coordination logic into a standalone **Writer** library. The
Writer handles LSN assignment, fan-out to replicas, and quorum
tracking. It is embedded in the user application and communicates with
an in-cluster **coordinator** for replica set discovery, epoch
changes, and build coordination.

The existing leader-based operator (`kuberic-operator`) is unchanged
and continues to work for its use cases.

> **Scope:** Single writer only. Multi-writer extensions are out of
> scope for this design.

> Part of the [Kuberic Design](../kuberic-replicator-design.md).
> Related: [Leaderless Quorum Research](../../background/leaderless-quorum-replication.md),
> [WAL Persistence](wal-persistence.md)
>
> **This is a new, separate system.** The existing `kuberic-operator`
> (leader-based, SF-style) is not modified. Users choose one or the
> other per workload.

---

## Motivation

### Current Write Path (2 round-trips)

Today, a client write traverses:

```
Client  ──gRPC──►  Primary Pod  ──gRPC──►  Secondary₁  ──ACK──►  Primary  ──reply──►  Client
                                ──gRPC──►  Secondary₂  ──ACK──►     │
                                                                     │
                                         (wait for W ACKs)          │
                                                                     │
                                         ◄───── LSN reply ──────────┘
```

**Hops:** Client → Primary (1) → Secondaries (2) → Primary (3) →
Client (4). The critical path is **2 sequential network round-trips**:
client↔primary + primary↔fastest-secondary.

The code path is:

1. `KvServer::put()` serializes the op
   (`examples/kvstore/src/server.rs:52-78`)
2. `StateReplicatorHandle::replicate()` sends `ReplicateRequest` over
   an mpsc channel to the `WalReplicatorActor`
   (`kuberic-core/src/handles.rs:162-186`)
3. The actor assigns an LSN, stores in `ReplicationQueue`, registers
   with `QuorumTracker`, and calls `PrimarySender::send_to_all()`
   which enqueues to per-secondary unbounded channels
   (`kuberic-core/src/replicator/actor.rs:312-336`)
4. Background drain tasks bridge unbounded channels to gRPC
   `ReplicationStream` streams
   (`kuberic-core/src/replicator/primary.rs:123-130`)
5. Secondaries receive items, store them, and send `ReplicationAck`
   back (`kuberic-core/src/replicator/secondary.rs:167-240`)
6. ACK reader tasks call `QuorumTracker::ack()` — when quorum is met,
   the original `replicate()` future resolves with the LSN
   (`kuberic-core/src/replicator/quorum.rs:165-197`)

### The Bottleneck

The primary pod is a **serialization point**: every write must traverse
it, even though the primary's contribution to quorum is just "one more
ACK" — the same as any secondary. The primary adds latency (extra
network hop + channel + actor loop) and caps throughput (single actor
event loop processes all writes sequentially).

### Proposed Write Path (1 round-trip)

The Writer is embedded in the user app. It fans out directly to all
replicas and receives config from a coordinator.

```
                            ┌── Coordinator (in-cluster) ◄── watches ── Operator/CRD
                            │
User App + Writer ──config stream──┘
                  ──gRPC──►  Replica₁  ──ACK──┐
                  ──gRPC──►  Replica₂  ──ACK──┤  Writer waits for W ACKs
                  ──gRPC──►  Replica₃  ──ACK──┘
                                               │
                  ◄── LSN returned ────────────┘
```

**Critical path:** 1 network round-trip — Writer → W replicas → ACK.

**Expected latency improvement:** ~50-67% reduction for typical
3-replica deployments where primary↔secondary latency ≈ client↔pod
latency.

---

## Design

### Key Design Decisions

**Single writer → no roles.** With one Writer doing all writes, there
is no reason to distinguish Primary from Secondary. All replicas are
identical — they accept writes, validate epochs, store ops, ACK. The
Writer is the coordinator, but it is not a role assigned to a replica —
it's a library embedded in the user application.

**Single writer → trivial LSN assignment.** The multi-writer LSN
problem (range allocation, HLC, etc.) disappears entirely. The Writer
holds a simple monotonic counter: `next_lsn += 1`. Total ordering is
preserved — identical to today's primary-side counter.

**Single writer → no conflict resolution.** There is exactly one
source of writes, producing a single totally-ordered sequence of
operations. No concurrent conflicting writes, no vector clocks, no
LWW, no CRDTs. Every replica sees the same sequence.

**Writer communicates via coordinator.** The Writer may run anywhere
— inside or outside the K8s cluster. It must not depend on direct
access to the K8s API or the operator. Instead, the Writer connects
to a **coordinator** (a lightweight gRPC service running in-cluster)
over a single long-lived bidirectional stream. The coordinator
abstracts away CRD watches and operator interactions.

```
Writer (anywhere)  ──gRPC stream──►  Coordinator (in-cluster)  ──watches──►  CRD/Operator
                   ◄─ config events                             ◄─ updates ──
                   ── status reports ─►                         ── writes ──►
```

The coordinator can be a separate service or an extra gRPC port on
the operator itself.

### Architecture Overview

This is a **new, parallel system** — not a modification of the
existing `kuberic-operator`. The two systems share some low-level
components (proto definitions, `QuorumTracker` logic) but are
independently deployable.

```
Existing system (unchanged)          New system (this design)
─────────────────────────────        ────────────────────────────
kuberic-operator                     kuberic-writer-op
  ├─ Primary election                  ├─ Replica set management
  ├─ Failover / switchover             ├─ Epoch management
  ├─ ChangeRole lifecycle              ├─ Copy source selection
  └─ Write revocation                  └─ Coordinator (gRPC)

kuberic-core                         kuberic-writer (new crate)
  ├─ WalReplicatorActor                ├─ Writer (library)
  ├─ PrimarySender                     ├─ QuorumTracker
  ├─ QuorumTracker                     ├─ LSN counter
  └─ SecondaryReceiver                 └─ Coordinator client

Pod roles: Primary / Secondary       Pod roles: None (all equal)
                                      Writer: library in user app
                                      Coordinator: in-cluster service
```

| Component | Current location | New system location |
|---|---|---|
| **LSN assignment** | `WalReplicatorActor` (primary pod) | Writer (`next_lsn` counter) |
| **Quorum tracking** | `QuorumTracker` (primary pod) | Writer |
| **Fan-out to replicas** | `PrimarySender` (primary pod) | Writer |
| **Op application** | User service on primary | Each replica independently |
| **Replication queue** | `ReplicationQueue` (primary pod) | Each replica (local) |
| **Epoch/fencing** | Operator → primary (`data_loss` + `config` combined) | Split: `data_loss` + `writer_epoch` (fenced) + `config_version` (informational) |
| **Pod roles** | Primary / Secondary | **None** — all replicas equal |

### What the New System Does NOT Need

The following machinery from `kuberic-operator` / `kuberic-core` is
**not present** in the new system (the existing system keeps all of
it):

| Not needed in new system | Exists in `kuberic-core` | Why it existed |
|---|---|---|
| `ChangeRole` lifecycle event | `actor.rs`, `pod.rs` | Transition Primary↔Secondary |
| `AccessStatus::NotPrimary` | `handles.rs:166` | Gate writes on non-primary |
| `PrimarySender` | `primary.rs` | Primary → secondary fan-out |
| `WalReplicatorActor` data path | `actor.rs:312-336` | LSN assignment, quorum, fan-out |
| `revoke_write_status()` RPC | `grpc/server.rs` | Write revocation before switchover (A2) |
| Switchover rollback (A3) | `pod.rs` | Re-promote old primary on failure |
| SF Phase 4 epoch distribution | `pod.rs` | Primary distributes epoch to secondaries after promotion |
| Primary election / failover | `kuberic-operator` | Elect new primary on failure |
| Switchover orchestration | `kuberic-operator` | Controlled primary migration |
| `write_status` / `read_status` | `PartitionState` | Per-pod access gating (4-state enum tied to primary role) |

### Component Design

#### 1. Writer Library (`kuberic-writer`)

The core replication coordinator, packaged as a Rust library that the
user application links and calls directly:

```rust
/// The Writer — replication coordinator. Embedded in the user app.
/// Connects to replicas via gRPC and to the coordinator for
/// discovery and lifecycle events.
pub struct Writer {
    /// Connections to all replicas (gRPC channels)
    replicas: Vec<ReplicaConnection>,
    /// Partition config: epoch, replica set, write quorum
    config: Arc<PartitionConfig>,
    /// Simple monotonic counter (single writer — no contention)
    next_lsn: AtomicI64,
    /// Quorum tracker (reused from kuberic-core)
    quorum: QuorumTracker,
    /// Replication queue — retains ops for replay to new replicas
    replication_queue: ReplicationQueue,
    /// Bidirectional gRPC stream to coordinator
    config_stream: CoordinatorStream,
    /// Assigned by coordinator, stored locally for reconnect identity
    writer_id: String,
    /// Opaque token from coordinator, included in every ReplicationItem
    session_token: Bytes,
}

impl Writer {
    /// Write data to quorum. Returns assigned LSN.
    ///
    /// 1. Assign LSN (next_lsn += 1)
    /// 2. Buffer in ReplicationQueue
    /// 3. Send ReplicationItem to all N replicas in parallel
    /// 4. Wait for W ACKs
    /// 5. Return LSN to caller
    pub async fn replicate(&self, data: Bytes) -> Result<Lsn> { ... }
}
```

**Writer bootstrap:** When a new Writer connects, the coordinator
sends the current `ConfigUpdate` which includes the partition's
`committed_lsn` (persisted in the CRD status by the operator from
the previous Writer's reports). The new Writer sets
`next_lsn = committed_lsn + 1` and queries each replica for its
`received_lsn` to catch up on any ops that were committed but not
yet confirmed. This ensures no LSN gaps or duplicates after Writer
restart.

**LSN assignment is trivial:** Single writer means a plain atomic
counter — `next_lsn.fetch_add(1, Ordering::Relaxed)`. Identical to
today's `next_lsn += 1` in `actor.rs:315`, just moved to the Writer.
Globally sequential, contiguous, no gaps.

#### 2. Coordinator

A lightweight gRPC service running **in-cluster** (as a standalone
service or an extra port on the operator). It bridges the Writer
(which may be anywhere) to the K8s control plane.

```protobuf
service Coordinator {
    // Bidirectional stream: Writer sends connect + status,
    // coordinator pushes config events.
    rpc WriterSession(stream WriterMessage) returns (stream ConfigEvent);
}

message WriterMessage {
    oneof message {
        WriterConnect connect = 1;       // first message on stream open
        WriterProgress progress = 2;     // periodic status report
        WriterShutdown shutdown = 3;     // cooperative graceful shutdown
    }
}

message WriterConnect {
    string writer_id = 1;               // empty on first-ever connect
    string partition_id = 2;            // which partition to write to
}

message ConfigEvent {
    oneof event {
        ConfigUpdate config_update = 1;   // epoch, replica set, write quorum (CC+PC)
        AddReplica add_replica = 2;       // new replica ready, copy_lsn = X
        RemoveReplica remove_replica = 3; // replica removed from set
        BuildInProgress build_start = 4;  // defer queue GC
        BuildComplete build_complete = 5; // resume queue GC
        PauseWrites pause_writes = 6;     // Writer must reject replicate() calls
        ResumeWrites resume_writes = 7;   // Writer resumes accepting writes
    }
}

message ConfigUpdate {
    Epoch epoch = 1;
    ReplicaSet current_config = 2;        // CC — current members + write quorum
    ReplicaSet previous_config = 3;       // PC — non-empty during reconfiguration
    string writer_id = 4;                 // assigned by coordinator
    bytes session_token = 5;              // opaque token for replica authentication
}

message WriterProgress {
    int64 committed_lsn = 1;     // Writer's quorum-committed LSN
    int64 highest_lsn = 2;       // Writer's highest assigned LSN
    bool caught_up = 3;          // all pending ops committed + must_catch_up satisfied
    // (stream liveness doubles as heartbeat)
}

message WriterShutdown {
    int64 committed_lsn = 1;     // final committed LSN at shutdown
}
```

##### Writer Identity and Authentication

The Writer is an external client — the coordinator must know who it
is and whether it's the same Writer reconnecting or a different one.

**Writer ID:** An opaque UUID assigned by the coordinator on first
connect. The Writer stores it locally and presents it on reconnect.

```
First connect:
  Writer → Coordinator:  WriterConnect { writer_id: "" }
  Coordinator:           Assigns writer_id = new UUID
                         Stores in CRD status
                         Sends ConfigUpdate { writer_id, session_token, ... }
  Writer:                Stores writer_id locally

Reconnect (same Writer):
  Writer → Coordinator:  WriterConnect { writer_id: "<stored UUID>" }
  Coordinator:           Matches against last known writer_id
    + clean shutdown →   Same epoch, resume (no fencing)
    + crash →            Bump writer_epoch (crash fencing)

Takeover (different Writer):
  Writer → Coordinator:  WriterConnect { writer_id: "" or unknown }
  Coordinator:           No match → new Writer
                         Bump writer_epoch, assign new writer_id
                         Fence old Writer's stale writes
```

**Session token:** On each `ConfigUpdate`, the coordinator issues an
opaque `session_token` (e.g., HMAC of `writer_id + writer_epoch +
timestamp`, signed by the coordinator). The Writer includes this
token in every `ReplicationItem`:

```protobuf
message ReplicationItem {
    int64 data_loss_number = 1;
    int64 writer_epoch = 2;
    int64 lsn = 3;
    bytes data = 4;
    int64 committed_lsn = 5;
    bytes session_token = 6;      // proves Writer identity to replicas
}
```

Replicas receive the valid `session_token` from the coordinator via
`update_epoch()` (the coordinator is in-cluster, trusted). On each
incoming `ReplicationItem`, the replica validates:
1. Epoch check: `(data_loss_number, writer_epoch) >= current` (ordering)
2. Token check: `session_token == expected_token` (identity)

A rogue client without a valid session token is rejected even if it
guesses the correct epoch values. The token rotates on every
`writer_epoch` bump, so a stolen token from an old Writer is
invalidated when the epoch changes.

**How it works:**

1. Writer opens `WriterSession` stream and sends `WriterConnect`
   with its stored `writer_id` (or empty on first connect).
2. Coordinator validates identity (see above), assigns/confirms
   `writer_id`, generates `session_token`, pushes `ConfigUpdate`.
3. Writer stores `writer_id`, connects to all replicas, includes
   `session_token` in every `ReplicationItem`.
4. When the operator changes config (epoch bump, add/remove replica,
   build), the coordinator pushes the relevant `ConfigEvent`.
5. Writer reacts: updates its connection pool, replays from queue for
   new replicas, adjusts quorum tracker.
6. Writer periodically (and on each commit) sends `WriterProgress`
   back — the coordinator forwards this to the operator / CRD status.
7. **Liveness:** If the stream disconnects without a prior
   `WriterShutdown`, the coordinator treats it as a crash (see
   Writer Lifecycle below).

##### Writer Lifecycle Protocol

The coordinator distinguishes three Writer restart scenarios based
on whether a `WriterShutdown` message was received before disconnect:

**Graceful shutdown → clean restart (no fencing):**

```
Writer                              Coordinator
──────                              ───────────
1. Drain in-flight ops
   (wait for pending replicate() to complete)
2. Send WriterShutdown{committed_lsn=X}  ──►
                                    3. Record: "clean shutdown at LSN X"
                                       No epoch bump, no update_epoch to replicas
4. Close stream, exit process

... later ...

5. Writer reconnects  ──────────────►
                                    6. Last shutdown was clean
                                       Send ConfigUpdate with SAME writer_epoch
                                       Resume LSN = committed_lsn + 1
7. Writer resumes, no truncation
```

**Crash → fenced restart (epoch bump):**

```
Writer                              Coordinator
──────                              ───────────
1. Process crashes / network dies
   (no WriterShutdown sent)
                                    2. Stream disconnect detected
                                       No WriterShutdown received
                                       → treat as crash
                                    3. Bump writer_epoch
                                    4. Push update_epoch to all replicas
                                       (replicas truncate uncommitted ops)
                                    5. Record: "crashed, new writer_epoch=Y"

... later ...

6. Writer reconnects  ──────────────►
                                    7. Last shutdown was crash
                                       Send ConfigUpdate with NEW writer_epoch
                                       Resume LSN = last known committed_lsn + 1
8. Writer resumes with new epoch
```

**Takeover (different Writer connects while old is alive):**

Same as crash — the coordinator cannot prove the old Writer is dead,
so it bumps `writer_epoch` and fences. The old Writer's stale writes
are rejected by replicas that have received the epoch bump.

**Why no lease?** The cooperative protocol handles the graceful case
(no unnecessary fencing), and crash fencing (immediate epoch bump)
handles the crash case identically to the existing leader-based
system's failover model. A lease would add recovery latency (must
wait for expiry) without meaningful safety improvement, since
eventual epoch fencing already ensures uncommitted ops are truncated.

##### Reconfiguration via Coordinator

Adding or removing a replica follows the same three-step
configuration dance as today, but driven by `ConfigEvent`s instead
of direct RPC calls on the primary:

```
                                              Writer
                                              ──────
1. Operator completes copy, tells coordinator
   "add replica 3 at copy_lsn=50"

   Coordinator → Writer:  AddReplica(id=3, addr, copy_lsn=50)
                                              Writer connects to replica 3
                                              Replays queue from LSN 51
                                              Starts live send_to_all

2. Operator enters dual-config

   Coordinator → Writer:  ConfigUpdate(
                            CC={1,2,3} quorum=2,
                            PC={1,2} quorum=2,
                            must_catch_up={3})
                                              Writer enters dual-config:
                                              ops must be ACKed by quorum
                                              from BOTH CC and PC

   Writer → Coordinator:  WriterStatus(caught_up=false)
                          ...
   Writer → Coordinator:  WriterStatus(caught_up=true)
                                              Replica 3 has ACKed all ops

3. Operator finalizes

   Coordinator → Writer:  ConfigUpdate(
                            CC={1,2,3} quorum=2,
                            PC=∅)
                                              Writer exits dual-config
                                              GC queue up to committed_lsn
```

This is identical to today's `update_catch_up_configuration` →
`wait_for_catch_up_quorum` → `update_current_configuration` sequence,
just expressed as stream events instead of RPCs.

##### Write Gating

In the current system, `AccessStatus` is a 4-state enum tied to
primary role transitions:

| Status | Meaning |
|---|---|
| `Granted` | Primary, writes allowed |
| `NotPrimary` | Not the primary — reject |
| `NoWriteQuorum` | Can't reach enough secondaries |
| `ReconfigurationPending` | Mid-reconfiguration pause |

In the Writer-based system, most of these don't apply:

- **`NotPrimary`** — irrelevant (no primary role).
- **`NoWriteQuorum`** — happens naturally. If the Writer can't get W
  ACKs, `replicate()` times out / fails. No explicit status needed.
- **`ReconfigurationPending`** — the Writer continues writing during
  reconfiguration. Dual-config quorum ensures both old and new
  configurations are satisfied. No pause needed.

The only scenario requiring an explicit write pause is **data loss
recovery**, where the operator needs to freeze all replica state for
a consistent rebuild:

| Scenario | Write pause? | Why |
|---|---|---|
| Replica added | No | Writer continues, queue buffers gap ops |
| Replica removed | No | Writer drops connection, continues |
| Writer failure | No | Old Writer is dead, epoch fences stale writes |
| Normal quorum loss | No | `replicate()` naturally fails (can't get W ACKs) |
| **Data loss recovery** | **Yes** | Must freeze state for consistent rebuild from canonical |

The coordinator sends `PauseWrites` before data loss recovery begins
and `ResumeWrites` after the rebuild completes. The Writer rejects
all `replicate()` calls with an error while paused.

```
Data loss recovery:
  Coordinator → Writer:  PauseWrites
                         Writer rejects replicate() calls
  Operator rebuilds all replicas from canonical survivor
  Coordinator → Writer:  ResumeWrites(new epoch, new replica set)
                         Writer resumes with new data_loss_number
```

This replaces the entire `AccessStatus` enum with a simple boolean.

**Why not direct K8s API?** The Writer may run outside the cluster
(user's laptop, different cloud, CI pipeline). It should not need
K8s credentials or CRD access. The coordinator is the single
in-cluster contact point — one gRPC endpoint to reach.

#### 3. Replica Pod (Roleless)

All pods run identical code. No Primary/Secondary role assignment,
no `ChangeRole` events.

```protobuf
service ReplicaData {
    // Writer sends items, replica ACKs each one
    rpc Write(stream ReplicationItem) returns (stream ReplicationAck);

    // Copy protocol: operator designates a source replica
    rpc GetCopyContext(GetCopyContextRequest) returns (GetCopyContextResponse);
    rpc CopyStream(stream CopyItem) returns (CopyStreamResponse);
}
```

Each replica:
- Accepts `ReplicationItem` (containing epoch + LSN + data +
  session_token)
- Validates epoch (fencing: rejects stale `writer_epoch` or
  `data_loss_number`)
- **Validates session token** (identity: rejects items without a
  valid token matching the one received from coordinator via
  `update_epoch`)
- Stores in local `ReplicationQueue` / WAL
- Applies op to user state via `StateProvider`
- Sends `ReplicationAck` back to the Writer

The existing `SecondaryReceiver::accept_item()` epoch validation and
op storage logic (`secondary.rs:95-124`) is reused almost verbatim —
it already does exactly what a roleless replica needs.

#### 4. Epoch Design

##### Current Epoch (Leader-Based)

Today's epoch has two components, both fencing via a single comparison:

```rust
pub struct Epoch {
    pub data_loss_number: i64,       // quorum loss events
    pub configuration_number: i64,   // every reconfiguration
}
```

`configuration_number` is bumped on every change: failover, switchover,
add replica, remove replica. It fences the old primary after failover.
But it also gets bumped for replica membership changes that don't change
the writer — creating unnecessary write stalls.

##### New Epoch (Writer-Based)

The Writer-based system splits the epoch into three independent
numbers, because three independent concerns exist:

```rust
pub struct Epoch {
    /// Incremented when the operator declares data loss (quorum loss
    /// recovery with fewer than W surviving replicas). Signals that
    /// previously committed operations may be gone.
    pub data_loss_number: i64,

    /// Incremented when the Writer identity changes (Writer failure
    /// and takeover). Fences stale Writer's in-flight writes.
    pub writer_epoch: i64,

    /// Incremented when the replica set membership changes (add,
    /// remove, rebuild). NOT checked on writes — informational
    /// for the Writer to update its connection pool.
    pub config_version: i64,
}
```

| Number | Bumped when | Fences what | Checked by replicas on write? |
|---|---|---|---|
| `data_loss_number` | Operator declares data loss | All prior epochs | **Yes** — supersedes everything |
| `writer_epoch` | Writer dies, new Writer takes over | Stale Writer's writes | **Yes** — rejects old Writer |
| `config_version` | Replica added / removed / rebuilt | Nothing (informational) | **No** — Writer uses it to update pool |

**Replica fencing check:** Replicas compare incoming
`(data_loss_number, writer_epoch)` against their current values.
An item is rejected if either component is stale:

```rust
fn accept_item(&self, item: &ReplicationItem) -> Result<(), Status> {
    let current = self.current_epoch.lock().unwrap();
    let item_epoch = (item.data_loss_number, item.writer_epoch);
    let my_epoch = (current.data_loss_number, current.writer_epoch);

    if item_epoch < my_epoch {
        return Err(Status::failed_precondition("stale epoch"));
    }

    // Identity check: session token must match (received from coordinator)
    if item.session_token != self.expected_session_token {
        return Err(Status::unauthenticated("invalid session token"));
    }

    // ... store op
}
```

`config_version` is not part of this check — a replica addition
should not stall writes from the existing Writer.

##### When Each Number Changes

**Writer failure → `writer_epoch` bumped:**

```
1. Writer dies (coordinator detects stream disconnect)
2. Operator bumps writer_epoch
3. Operator pushes new epoch to all replicas (update_epoch RPC)
4. Replicas truncate uncommitted ops beyond committed_lsn
   (same rollback logic as today's update_epoch, secondary.rs:74-85)
5. New Writer connects to coordinator, receives current epoch
6. Old Writer's late-arriving writes → rejected (stale writer_epoch)
```

**Replica added → `config_version` bumped:**

```
1. Operator completes copy to new replica
2. Operator bumps config_version
3. Coordinator pushes ConfigUpdate (new replica set) to Writer
4. Writer adds new replica to its connection pool, replays from queue
5. NO epoch push to existing replicas — no write stall
```

**Replica removed → `config_version` bumped:**

```
1. Operator removes replica from set
2. Operator bumps config_version
3. Coordinator pushes ConfigUpdate to Writer
4. Writer removes replica from its pool
5. NO epoch push to existing replicas — no write stall
```

**Quorum loss → `data_loss_number` bumped:**

```
1. Multiple replicas die, fewer than W remain
2. Operator waits (quorum loss timeout)
3. If quorum doesn't recover, operator drops dead replicas and
   declares data loss
4. Operator bumps data_loss_number (supersedes all writer_epochs)
5. Operator pushes new epoch to surviving replicas
6. Surviving replicas receive on_data_loss() callback
   → state provider can restore from backup or accept the gap
7. Coordinator pushes new epoch to Writer (or new Writer connects)
8. Writes resume from whatever LSN the survivors have
```

##### Proto Changes

The `ReplicationItem` gains a `writer_epoch` field. `config_version`
is not included in items — it's only in `ConfigEvent` messages
between coordinator and Writer.

```protobuf
message ReplicationItem {
    int64 data_loss_number = 1;   // was epoch_data_loss
    int64 writer_epoch = 2;       // was epoch_config (repurposed)
    int64 lsn = 3;
    bytes data = 4;
    int64 committed_lsn = 5;
    // config_version NOT included — not checked on writes
}
```

##### Data Loss Recovery

Data loss occurs when fewer than W replicas survive — the quorum
contract is broken, and previously committed operations may be gone
from the surviving replicas.

**Leader-based (today):** The operator promotes the best surviving
secondary as the new primary and calls `on_data_loss()` on that one
replica. The new primary becomes the canonical state, and all other
replicas are rebuilt from it via the copy protocol. Only one replica
handles the data loss callback.

**Writer-based:** There is no primary to promote, but the recovery
follows the same pattern — one survivor becomes the canonical state:

```
Before:  Replica₁ (LSN=100)  Replica₂ (LSN=98)  Replica₃ (dead)
         Writer committed_lsn=99

1. Quorum lost (only 2 of 3 replicas alive, need W=2 but
   committed_lsn=99 means ops 99-100 may not both be on
   both survivors)

2. Operator waits for quorum loss timeout

3. Operator picks survivor with highest LSN → Replica₁
   (has the most complete state)

4. Operator calls on_data_loss() on Replica₁ ONLY
   → Replica₁'s state provider can:
     a. Accept the gap (do nothing — most common)
     b. Restore from external source (backup, another service)
     c. Report StateChanged if it modified state

5. Operator bumps data_loss_number

6. Operator rebuilds ALL other replicas from Replica₁:
   - Replica₂ → closed, rebuilt via copy protocol from Replica₁
   - Replica₃ → new pod, built via copy protocol from Replica₁
   (same as adding new replicas — Replica₁ is the copy source)

7. After rebuild, operator pushes new epoch (with bumped
   data_loss_number) to all replicas

8. Coordinator pushes new epoch to Writer (or new Writer connects)

9. Writer resumes with new data_loss_number, new replica set,
   LSN continues from Replica₁'s highest LSN

After:   Replica₁ (canonical, LSN=100)
         Replica₂ (rebuilt from Replica₁, LSN=100)
         Replica₃ (rebuilt from Replica₁, LSN=100)
         Writer starts at next_lsn=101
```

**Key points:**

- **Only one replica gets `on_data_loss()`** — the most-complete
  survivor, chosen by highest LSN. Same as the leader-based system.
- **All other survivors are rebuilt** from the canonical replica via
  the normal copy protocol. This ensures all replicas converge to
  the same state. A survivor at LSN=98 when the canonical is at
  LSN=100 could have divergent ops (ops the canonical saw but this
  one didn't, or vice versa). Rebuilding eliminates this ambiguity.
- **No external restore required by default.** The most common
  `on_data_loss()` implementation is "accept the gap" — return
  `StateChanged = false`. The surviving data is the new truth.
- **External restore is optional.** If the user's state provider has
  access to backups or an external source, `on_data_loss()` is the
  hook to restore from it. This is application-specific.
- **The `StateProvider` trait is unchanged** — the same
  `on_data_loss() → bool (state_changed)` callback works identically
  in both systems.

#### 5. Quorum Tracking in the Writer

The Writer embeds the same `QuorumTracker` logic from
`kuberic-core/src/replicator/quorum.rs`:

- `register(lsn, reply)` — register pending op
- `ack(lsn, replica_id)` — record ACK from a replica
- `is_quorum_met()` — check if W replicas have ACKed

**No self-ACK.** Unlike the leader-based system where the primary
counts its own local persist as one ACK (`quorum.rs:131-132`), the
Writer does NOT self-ACK — it holds no data. It waits for W out of N
**replica** ACKs. For N=3, W=2: any 2 replicas ACKing is sufficient.
This means W must be set with the understanding that all ACKs come
from replicas (the write quorum is not inflated by a phantom
self-ACK).

#### 6. Committed LSN Propagation

Today the primary piggybacks `committed_lsn` on `ReplicationItem`
so secondaries can track quorum progress. In the Writer model:

- The **Writer** knows `committed_lsn` (its `QuorumTracker` tracks
  it). It includes `committed_lsn` in every `ReplicationItem` —
  same field, same proto, just set by the Writer instead of the
  primary actor.
- Replicas use `committed_lsn` for GC and deferred application,
  exactly as they do today.

##### Lagging committed_lsn on Writer Crash

**Problem:** `committed_lsn` is piggybacked on the **next**
`ReplicationItem` after the commit. If the Writer commits LSN=50
(gets W ACKs) and crashes before sending LSN=51, no replica ever
learns that `committed_lsn=50`. All replicas think
`committed_lsn=49`. On epoch bump, replicas truncate LSN=50 — a
committed op is lost.

The existing leader-based system avoids this because the primary
doesn't truncate its own ops, and the new primary is chosen by
highest LSN. In the Writer system, all replicas truncate based on
their local `committed_lsn`, which lags.

**Fix:** The Writer reports `committed_lsn` to the coordinator on
every commit (via the `WriterProgress` stream message, already
designed — fire-and-forget on the open coordinator stream). The
coordinator persists it in the CRD status. On epoch bump, the
coordinator pushes the **authoritative** `committed_lsn` to replicas
along with the new epoch:

```protobuf
message UpdateEpochRequest {
    Epoch new_epoch = 1;
    int64 authoritative_committed_lsn = 2;  // from coordinator
}
```

Replicas truncate based on this value, not their local stale copy.
The Writer should send `WriterProgress` **before** returning the LSN
to the user, ensuring the coordinator always has a value ≥ what the
user has seen. This is a fire-and-forget send on the already-open
coordinator stream — negligible cost.

An alternative approach — picking an "authority" replica by highest
LSN (the same technique the existing leader-based system uses for
failover) — was considered and rejected. The authority approach
keeps all ops on the highest-LSN survivor, including ops that the
client was **explicitly told failed** (`Err(NoWriteQuorum)`). In the
existing leader-based system, the primary crash IS the client error
(in-doubt status), so keeping everything is valid. In the Writer
system, the client can receive a definitive failure and THEN the
Writer crashes — the system must honor that failure by not retaining
the data.

See [peer-repair-replication.md](peer-repair-replication.md)
(abandoned — preserved for reference) for the full analysis with
scenario traces, the authority-based alternative analysis, and the
response-consistency argument.

#### 7. Replica Building (Copy + Catch-Up)

Replica building is the most nuanced part of the Writer-based system
because the responsibilities that today live on a single primary are
now split across two actors:

- **Copy source replica** — has the data (`StateProvider`), produces
  the state snapshot, streams it to the new replica.
- **Writer** — has the `ReplicationQueue` with ops written during the
  copy window, handles the replay gap and live replication.

##### How It Works Today (Leader-Based)

In the current system, the primary does everything
(`replicator/copy.rs`, `replicator/actor.rs:257-275`,
`replicator/primary.rs:62-140`):

```
[0 ─────── copy_lsn]              Copy stream (StateProvider snapshot)
           (copy_lsn ── highest]   Replay from ReplicationQueue
                       (highest ── ∞)  Live replication (send_to_all)
```

1. Operator tells primary: `build_replica(new_secondary_info)`
2. Primary spawns copy task (`copy.rs:18`):
   a. Connects to new secondary's gRPC data plane
   b. `GetCopyContext` → secondary returns its current context
   c. `GetCopyState(up_to_lsn=committed_lsn, copy_context)` →
      primary's StateProvider produces snapshot
   d. `CopyStream` → primary pushes all state items to secondary
   e. Records `copy_lsn = max(snapshot LSNs)` via
      `state.set_copy_lsn(replica_id, copy_lsn)`
3. Operator promotes secondary: `change_role(ActiveSecondary)`
4. Operator calls `update_catch_up_configuration` on primary
5. Primary actor (`actor.rs:136-221`):
   a. `add_secondary(id, addr)` → opens gRPC replication stream,
      spawns ACK reader + drain tasks
   b. `take_copy_lsn(replica_id)` → retrieves boundary
   c. `replication_queue.ops_from(copy_lsn + 1)` → gets gap ops
   d. `send_to_one()` for each gap op → replays to new secondary
   e. From now on, `send_to_all()` includes the new secondary
6. Operator calls `wait_for_catch_up_quorum(Write)`
7. Operator calls `update_current_configuration` → finalizes quorum

**Critical invariant:** The three ranges `[0, copy_lsn]`,
`(copy_lsn, highest_lsn]`, `(highest_lsn, ∞)` are gapless because
the primary is both the copy source AND the replication queue holder.
During the copy window, new `replicate()` calls keep arriving and
buffering in the queue. The `copy_lsn` is recorded atomically with
the snapshot. No ops are lost.

##### How It Works in the Writer-Based System

The copy source and the queue holder are **separate processes**. The
`copy_lsn` must be communicated between them.

```
Operator              Coordinator       Copy Source Replica     New Replica           Writer
────────              ────────────       ────────────────────    ───────────           ──────
                                                                                      (continuously accepting
                                                                                       replicate() calls,
                                                                                       buffering in queue)

1. Pick copy source                                                                   
   (highest committed_lsn)
                      ◄── BuildInProgress ──────────────────────────────────── push ──►
                                                                                      (defers queue GC)
2. Tell copy source:
   build to new ──────────────────────►
                                       3. Connect to new replica's gRPC
                                       4. GetCopyContext ──────► Returns context
                                       5. GetCopyState
                                          (local StateProvider
                                           snapshot up to
                                           committed_lsn)
                                       6. CopyStream ─────────► Receives full state
                                                                 via copy_stream
                                          copy_lsn = max(snapshot LSNs)
◄── done, copy_lsn ───────────────────

7. Tell Writer via
   coordinator:
   add replica,       ── AddReplica ─────────────────────────────────────────── push ──►
   copy_lsn = X                                                                       8. Record copy_lsn
                                                                                       9. Connect to new
                                                                                          replica (Write gRPC)
                                                                                       10. Replay queue from
                                                                                           copy_lsn + 1 via
                                                                                           send_to_one()
                                                                 ◄── replayed ops ────
                                                                 ◄── live ops ──────── 11. send_to_all() now
                                                                                            includes new
                                                                 ──── ACKs ───────────►

                      ◄── BuildComplete ─────────────────────────────────────── push ──►
                                                                                       (resumes queue GC)
12. Quorum catch-up    ── ConfigUpdate ──────────────────────────────────────── push ──►
    (update_catch_up)                                                                  (enters dual-config)
13. Wait for catch-up  ◄── WriterStatus (committed_lsn) ────────────────── reports ────
14. Finalize config    ── ConfigUpdate ──────────────────────────────────────── push ──►
                                                                                       15. replication_queue.gc()
```

##### Preserving the Zero-Gap Invariant

The three ranges remain gapless:

```
[0 ─────── copy_lsn]              Copy source replica's snapshot
           (copy_lsn ── highest]   Writer's ReplicationQueue replay
                       (highest ── ∞)  Writer's live send_to_all()
```

This works because:

1. **The Writer holds all ops.** Every `replicate()` call goes through
   the Writer, which buffers in its `ReplicationQueue`. The queue
   contains all ops from the beginning (or from the last GC point).

2. **The copy source snapshots at `committed_lsn`.** The copy source
   replica has applied all ops up to `committed_lsn` (which the Writer
   piggybacks on every `ReplicationItem`). Its `StateProvider`
   produces a snapshot through this LSN. The highest LSN in the
   snapshot is `copy_lsn`.

3. **`copy_lsn ≤ committed_lsn ≤ highest_lsn`.** Since the copy
   source snapshots at `committed_lsn`, and the Writer's queue has
   everything from `committed_lsn` onward (and more), the replay
   range `(copy_lsn, highest_lsn]` is fully covered by the queue.

4. **The Writer has not GC'd past `copy_lsn`.** The Writer must not
   GC its queue below `copy_lsn` while a build is in progress.
   **Critically, `BuildInProgress` is sent to the Writer BEFORE the
   copy starts** (see step 1 in the diagram above). This ensures the
   Writer freezes its GC boundary before the copy source captures its
   snapshot. Without this ordering guarantee, the Writer could GC ops
   that the new replica needs for catch-up.
   GC until the build completes and `update_current_configuration`
   finalizes the quorum.

##### Difference from Today

| Aspect | Leader-based (today) | Writer-based |
|---|---|---|
| Copy source | Primary (implicit) | Any replica (operator picks) |
| Queue holder | Primary | Writer |
| `copy_lsn` transfer | In-process (`state.set_copy_lsn`) | Cross-process (operator relays) |
| Concurrent writes during copy | Primary buffers in queue | Writer buffers in queue (same) |
| Replay gap ops | Primary's actor `send_to_one` | Writer's `send_to_one` |
| Live replication after build | Primary's `send_to_all` | Writer's `send_to_all` |
| GC coordination | Implicit (primary knows) | Explicit (operator signals build-in-progress) |

##### StateProvider Callbacks for Copy

Each replica still implements the same `StateProvider` callbacks:

- **`GetCopyContext`** — the new replica returns its current state
  context (what it already has, typically empty for a fresh replica).
  Called by the copy source via gRPC.
- **`GetCopyState`** — the copy source replica receives `up_to_lsn`
  + `copy_context`, produces a state snapshot. Called by the copy
  source's own build task.
- **Copy stream** — the new replica receives state items via
  `CopyStream` RPC and applies them via its `OperationStream`
  (same as today's `copy_op_tx` → `copy_stream`).

The copy source replica needs the ability to run `build_replica`
(today only the primary can). This means the `run_build_replica_copy`
logic moves from a primary-only code path to a general replica
capability — any replica can serve as copy source when asked by the
operator.

#### 8. New Operator: `kuberic-writer-op`

A new, lightweight operator — separate from `kuberic-operator` — that
includes the coordinator as a built-in gRPC endpoint:

| Responsibility | `kuberic-operator` (existing) | `kuberic-writer-op` (new) |
|---|---|---|
| Replica set membership | ✓ | ✓ |
| Epoch management | ✓ | ✓ |
| Primary election | ✓ | — |
| Failover orchestration | ✓ | — |
| Switchover orchestration | ✓ | — |
| Write revocation (A2) | ✓ | — |
| Switchover rollback (A3) | ✓ | — |
| Copy source selection | Implicit (primary) | Explicit (pick replica with highest committed_lsn) |
| Config distribution | To primary → secondaries | Coordinator → Writer (stream) |
| Writer liveness | N/A | Coordinator detects stream disconnect |
| CRD | `KubericPartition` (existing) | `KubericWriterPartition` (new CRD) |

**Failure handling:** When a replica pod dies, `kuberic-writer-op`
removes it from the replica set, bumps the epoch, and pushes
`ConfigUpdate` + `RemoveReplica` to the Writer via the coordinator.
No failover, no election, no promotion. If W replicas are still
reachable, writes continue uninterrupted.

**Writer failure:** When the coordinator detects the Writer's stream
disconnect, the operator marks the partition as writer-less. A new
Writer instance can connect and receive the current config. The
operator bumps the epoch to fence any stale writes from the old
Writer.

#### 9. Simplified StateProvider Trait

The user-facing `StateProvider` trait drops role-related callbacks but
retains copy callbacks (every replica can be a copy source):

```rust
// Today's lifecycle events:
enum LifecycleEvent {
    Open { ... },
    ChangeRole { new_role: Role, ... },  // ← REMOVED
    CloseReplica,
    Abort,
}

// Writer-based model:
enum LifecycleEvent {
    Open { ... },
    CloseReplica,
    Abort,
}

// StateProvider events (retained — any replica may serve as copy source):
enum StateProviderEvent {
    UpdateEpoch { ... },
    GetLastCommittedLsn { ... },
    GetCopyContext { ... },       // new replica returns its context
    GetCopyState { ... },         // copy source produces snapshot
    OnDataLoss { ... },
}
```

No `Role` enum needed. No `ChangeRole` transitions. Each replica
opens, accepts writes, and closes. The `OperationStream` for applying
ops works identically — replicas receive ops and acknowledge them.

---

## New System vs. Existing System

The existing `kuberic-operator` + `kuberic-core` system is **not
modified**. The new system is built alongside it.

### New Components (to build)

| Component | Crate | Purpose |
|---|---|---|
| `Writer` | `kuberic-writer` (new) | Replication coordinator library (embedded in user app) |
| `CoordinatorClient` | `kuberic-writer` (new) | Writer-side coordinator stream client |
| `QuorumTracker` | Shared (extracted) | Reused from `kuberic-core`, made a shared dep |
| `ReplicaWriteServer` | `kuberic-writer-runtime` (new) | Roleless pod runtime accepting writes |
| `Coordinator` | `kuberic-writer-op` (new) | gRPC endpoint on operator for Writer communication |
| `kuberic-writer-op` | `kuberic-writer-op` (new) | Operator + coordinator (membership, epochs, builds) |
| `KubericWriterPartition` | CRD | New CRD for writer-based partitions |

### Not Present in New System

| Existing component | Why not needed |
|---|---|
| `PrimarySender` | Writer does fan-out |
| `WalReplicatorActor` data path | Writer assigns LSN, tracks quorum |
| `ChangeRole` lifecycle | No roles |
| `revoke_write_status()` | No primary to revoke from |
| Switchover / failover logic | No primary to fail over |
| `AccessStatus` gating | All replicas accept writes |
| Primary election | No primary to elect |

### Reused from Existing System

| Component | How reused |
|---|---|
| `ReplicationItem` / `ReplicationAck` proto | Same message format |
| `QuorumTracker` logic | Extracted to shared crate |
| Epoch fencing logic | Same `accept_item` validation; split into `writer_epoch` + `data_loss_number` |
| Copy protocol (gRPC) | Same RPCs; any replica can be copy source |
| `run_build_replica_copy` | Same logic, generalized from primary-only to any replica |
| `ReplicationQueue` | Writer holds the queue; each replica has local storage |
| `StateProvider` trait | Open/Close/Abort + copy callbacks (minus ChangeRole) |
| `OperationStream` | Same op delivery + acknowledge pattern |

---

## Latency Analysis

### Current: 2 Round-Trips

```
t=0    Client sends to Primary
t=RTT₁ Primary receives, assigns LSN, sends to Secondaries
t=RTT₁+RTT₂  Fastest secondary ACKs → quorum met
t=RTT₁+RTT₂  Primary sends LSN reply to Client
t=2·RTT₁+RTT₂  Client receives LSN

Total ≈ 2·RTT(client↔pod) + RTT(pod↔pod)
```

Assuming RTT(client↔pod) ≈ RTT(pod↔pod) ≈ R:
**Total ≈ 3R** (best case), **4R** (if secondaries are in different
racks).

### Proposed: 1 Round-Trip

```
t=0    Writer sends to all N replicas in parallel
t=max(RTT₁..RTTw)  W replicas ACK → quorum met

Total ≈ max(RTT to W-th fastest replica) ≈ R
```

For N=3, W=2: **Total ≈ RTT to 2nd-fastest replica ≈ R** (same
cluster) or **~1.5R** (cross-rack).

**Improvement: ~50-67% latency reduction** (when Writer is in the same
network as replicas). If the Writer runs outside the cluster (e.g.,
user laptop, different cloud region), the RTT to replicas may be
higher than intra-cluster pod↔pod latency, reducing or negating the
latency benefit. The primary advantage in that case is architectural
simplicity (no roles, no failover), not raw latency.

Note: The coordinator stream is long-lived and carries only
infrequent config events — it adds no latency to the write path.

---

## Applicability

The Writer-based system requires that write operations produce a
**self-contained, opaque replication payload** that any replica can
apply independently. The Writer ships bytes — it never touches the
data itself. This constraint determines which workloads fit.

### Suitable Workloads

| Workload | Why it works |
|---|---|
| **KV store** (kvstore example) | Write = serialized `KvOp::Put{key,value}`. Pure data blob. Writer ships it, each replica deserializes and applies independently. |
| **Application-level state machine** | Write = serialized command/event. Replicas are deterministic state machines that apply commands in LSN order. |
| **Event sourcing** | Write = domain event. Replicas append to their local event log. Projections are derived per-replica. |
| **Document store** | Write = serialized document mutation. Each replica applies the mutation to its local store. |

**Common trait:** The application computes the replication payload
*before* calling `writer.replicate()`. No local state is needed at the
Writer — the payload is complete and self-describing.

### Unsuitable Workloads

| Workload | Why it doesn't work |
|---|---|
| **Replicated SQLite** (sqlite example) | SQL execution is inseparable from the local DB file. WAL frames are captured by reading the local WAL *after* SQLite commits. The Writer would need to host the SQLite DB to execute SQL and capture frames — making it a primary in all but name. |
| **Any page/WAL-level replication** | Replication payload (pages, WAL frames) is produced as a side effect of local storage engine operations. Cannot be separated from the data. |
| **Workloads requiring local reads** | If the Writer must serve reads from the data it writes, it needs local state — defeating the "Writer has no data" principle. |

### SQLite → Use Leader-Based System

The existing `kuberic-operator` (leader-based, SF-style) is the
correct choice for replicated SQLite and similar workloads where the
write path is coupled to local storage. The primary hosts the SQLite
DB, executes SQL, captures WAL frames, and replicates them. This
coupling is inherent to SQLite's architecture, not a design limitation.

### Decision Rule

```
Can the app produce the replication payload WITHOUT local state?
  YES → Writer-based system (this design)
  NO  → Leader-based system (kuberic-operator)
```

---

## Trade-Offs

### Advantages

1. **~50-67% latency reduction** — eliminates the primary hop; writes
   go directly from the user app to replicas in parallel.
2. **No failover stall** — pod failure is just "one fewer replica in
   quorum." As long as W pods are reachable, writes continue
   uninterrupted. No election, no promotion delay.
3. **Dramatically simpler operator** — no primary election, no
   switchover, no write revocation, no rollback.
4. **Simpler pod code** — no role distinction, no ChangeRole
   transitions, no AccessStatus gating. All replicas run identical
   code.
5. **Simpler StateProvider for users** — no ChangeRole callback,
   no role-aware logic needed in user services.

### Disadvantages

1. **Fat client** — the user app must link the Writer library, maintain
   gRPC connections to all N replica pods and to the coordinator.
2. **More connections** — app opens N+1 connections (N replicas + 1
   coordinator) instead of 1 to a primary.
3. **Coordinator dependency** — the Writer depends on a reachable
   coordinator for initial bootstrap and ongoing config events. If
   the coordinator is unreachable, the Writer can continue writing
   with its last known config but cannot react to membership changes.
   The Writer should enforce a **max staleness timeout** — if no
   coordinator heartbeat is received within a configurable window,
   the Writer should pause writes to avoid operating on an
   arbitrarily stale configuration.
4. **Partial writes** — if the Writer crashes after sending to some
   replicas but before quorum, those replicas have ops that never
   committed. Needs GC (expiry or rollback of uncommitted ops by
   the operator on epoch bump).
5. **Read-after-write** — without a primary, reads go to any replica.
   The Writer (or the app using it) must either (a) track its own
   `committed_lsn` and pass it to reads, or (b) always read from a
   specific replica it knows has the latest data.
6. **Single writer only** — this system is designed for exactly one
   Writer. For multi-writer workloads, use the existing
   `kuberic-operator` (leader-based) system.

---

## Implementation Plan

All work is in **new crates and a new operator**. The existing
`kuberic-operator` and `kuberic-core` are not modified (except for
extracting `QuorumTracker` to a shared crate).

### Phase 1: Shared Components

Extract reusable pieces from `kuberic-core` into a shared crate
(`kuberic-common` or similar):
- `QuorumTracker` — quorum logic
- Proto definitions (`ReplicationItem`, `ReplicationAck`, copy RPCs)
- `ReplicationQueue` — in-memory op buffer
- `Epoch`, `Lsn`, `ReplicaId` types

The existing `kuberic-core` re-exports from the shared crate — no
behavioral change.

### Phase 2: Roleless Replica Runtime (`kuberic-writer-runtime`)

New crate for roleless pods:
- `Write` RPC accepting `ReplicationItem` streams, returning
  `ReplicationAck` streams
- Epoch validation (reuse `SecondaryReceiver::accept_item()` logic)
- Local `ReplicationQueue` / WAL storage
- `StateProvider` trait (Open/Close/Abort — no ChangeRole)
- `OperationStream` for user-side op application

### Phase 3: Writer Library (`kuberic-writer`)

New crate:
- `CoordinatorClient` — bidirectional gRPC stream to coordinator
- gRPC connection pool to all replicas
- `QuorumTracker` (from shared crate)
- `ReplicationQueue` (from shared crate)
- `next_lsn` atomic counter
- `Writer::replicate()` — the user-facing API
- `committed_lsn` tracking and propagation
- React to `ConfigEvent`s: add/remove replicas, epoch changes,
  build coordination (replay from queue on `AddReplica`)

### Phase 4: New Operator with Coordinator (`kuberic-writer-op`)

New operator binary with new CRD (`KubericWriterPartition`):
- Replica set membership management
- Epoch management
- Copy source selection (pick replica with highest committed_lsn)
- **Coordinator gRPC service** — `WriterSession` RPC with
  bidirectional streaming for Writer communication
- Writer liveness detection (stream disconnect → epoch bump)
- Pod health monitoring and membership updates

### Phase 5: Example Application

Build a kvstore example (`examples/kvstore-writer/`) using the
Writer library end-to-end: embedded Writer, coordinator, roleless
replicas.

---

## Open Questions

1. **Partial write GC:** If the Writer crashes mid-write, some
   replicas have uncommitted ops. On the next epoch bump, should
   replicas discard ops beyond the last known `committed_lsn`?
   This is analogous to SF's rollback-on-reconfiguration.

2. **Read consistency:** Should `kuberic-writer` provide a `read()`
   API that ensures read-after-write? Options: (a) Writer tracks
   `committed_lsn`, passes to read, replica waits; (b) always read
   from a specific replica; (c) leave reads to the user.

3. **Writer liveness and takeover:** When the coordinator detects
   Writer death (stream disconnect), how long should it wait before
   allowing a new Writer? The epoch bump fences stale writes, but
   there may be in-flight ops from the old Writer still arriving at
   replicas. A short grace period (or replica-side epoch rejection)
   handles this.

4. **Coordinator availability:** If the coordinator is temporarily
   unreachable, the Writer continues with stale config. What happens
   if a replica dies during this window? The Writer will get rejected
   writes (from the dead replica's connection) but won't know to
   remove it. It should treat connection failures as transient and
   degrade gracefully (write to remaining replicas if W is still
   reachable).

5. **Write-ahead for crash safety:** If a replica crashes after ACKing
   but before applying, is data lost? This ties into the
   [WAL Persistence](wal-persistence.md) design — replicas should
   persist before ACKing.

6. **Migration path:** Can a workload migrate from the existing
   leader-based system to the new Writer-based system? This would
   require a coordinated cutover: drain the old primary, start the
   new Writer with the current data state and LSN.

---

## Known Issues

Issues identified during design review (SoT premortem analysis) that
require further design work before implementation.

### KI-1: Dual-Writer Window During Takeover (inherited, not new)

When the old Writer dies and a new one starts, there is a window
where epoch propagation to replicas is not atomic. A partitioned-
but-alive "zombie" Writer could still send items to replicas that
haven't yet received the epoch bump.

**This is not a new problem.** The existing leader-based system has
the same window during failover: the operator bumps
`configuration_number`, pushes the new epoch to secondaries
best-effort (`driver.rs:369` — logs a warning and continues if
`update_epoch` fails on a secondary), and promotes the new primary.
There is a window where the old primary can still reach
not-yet-bumped secondaries.

The existing system accepts this because:
- Epoch propagation is **in-cluster** (operator → replicas), so it's
  fast (milliseconds)
- Once a replica receives the epoch bump, it truncates uncommitted
  ops from the old epoch (`secondary.rs:74-85`)
- The old primary's writes to not-yet-bumped replicas are
  **uncommitted** — they'll be truncated when the epoch arrives
- No committed data is corrupted — only uncommitted in-flight ops
  are at risk, and they're rolled back

The Writer-based system inherits the same model — the coordinator
pushes `update_epoch()` to replicas in-cluster (same path, same
speed). The only difference is that an out-of-cluster Writer may
stay alive longer than an in-cluster primary, but its stale writes
are still uncommitted and will still be truncated.

**Mitigated by the Writer Lifecycle Protocol** (see section 2,
Coordinator): graceful shutdown sends `WriterShutdown` before
disconnect, so the coordinator skips fencing on clean restarts.
Crash restarts trigger immediate `writer_epoch` bump, same as the
existing system's failover.

**Lease considered and not adopted.** A lease would add crash
recovery latency (must wait for expiry) without meaningful safety
improvement — epoch fencing already truncates uncommitted ops.

**Severity: consider** (inherited from existing system, not a new
risk). Improvements (lease, synchronous fence token) would benefit
both systems equally and can be designed as a shared enhancement.

### KI-2: Writer State Recovery During Build (inherited, simple fix)

The Writer's `ReplicationQueue` is in-memory. If the Writer crashes
during a replica build, the queue is lost and the zero-gap invariant
is broken for the in-progress build.

**This is the same as the existing system.** Today's primary also has
an in-memory `ReplicationQueue` (`queue.rs`). If the primary crashes
during a build, the queue is lost. The existing system handles this
by aborting the in-progress build: the operator detects primary
failure, triggers failover, and the new primary restarts the build
from scratch via `restart_secondary` → full `add_replica` (copy +
build). The partially-built secondary is closed and rebuilt.

**The Writer system does the same thing:**
1. Writer crashes → coordinator detects stream disconnect
2. Coordinator bumps `writer_epoch`, aborts any in-progress build
3. New Writer connects, gets current config
4. Operator restarts the build from scratch (new copy from a replica)

**Why "abort and retry" is acceptable:**
- Builds are infrequent (scale-up, pod restart)
- A crash during a build is even rarer
- Copy is the expensive part, and it's idempotent — restarting just
  repeats the copy. No data corruption risk.
- The alternative (Writer-side WAL or replica-side query) adds
  significant complexity for a rare edge case

**Severity downgraded to consider.** The existing system has lived
with this behavior. The "abort builds on Writer crash" solution is
the recommended approach — simple, matches existing behavior, no new
infrastructure needed.

The more complex solutions (Writer-side WAL, replica-side ops query)
remain available as future optimizations if build frequency or build
size makes retry unacceptable.

### KI-3: Read Path Design (should-fix)

The design covers the write path thoroughly but does not specify read
semantics. Without a primary, read-after-write consistency is
undefined. Options to design:
- Writer tracks `committed_lsn` and the app passes it to reads
- Replicas expose a `read_at(min_lsn)` API that waits for the
  requested LSN to be applied
- Reads are documented as eventually consistent (simplest but may
  cause user confusion)

### KI-4: Unbounded ReplicationQueue Growth (should-fix, simple mitigation)

During a replica build with GC deferred (`BuildInProgress`), or when
the coordinator is unreachable (no `BuildComplete` to resume GC), the
Writer's in-memory queue grows without bound. Since the Writer is
embedded in the user app, this can cause OOM in the user process.

**The existing system has this too** — the primary's in-memory
`ReplicationQueue` grows during builds. But the primary runs in a
dedicated pod with Kubernetes resource limits, so OOM only kills the
pod (which triggers failover). The Writer runs in the user's
process, where OOM is more damaging.

**Recommended mitigation: build timeout + queue size limit.**

1. **Build timeout:** The Writer starts a timer on `BuildInProgress`.
   If `BuildComplete` doesn't arrive within a configurable window
   (e.g., 5 minutes), the Writer notifies the coordinator that the
   build has timed out. The coordinator aborts the build and resumes
   GC. The build can be retried later. This matches the existing
   system's behavior — `build_replica` has a 30-second timeout
   (`copy.rs:23`), and the operator can retry.

2. **Queue size limit:** The Writer enforces a maximum queue size
   (configurable, e.g., 10,000 ops or 100MB). If the limit is
   reached during a build, `replicate()` returns an error
   (`QueueFull`) rather than OOMing. The user app can decide whether
   to retry, drop the write, or shut down cleanly. This is better
   than a silent OOM crash.

These are configuration options on the `Writer`, not protocol
changes. The coordinator doesn't need to know about them.

### KI-5: Zombie Writer Self-Fencing (should-fix, simple implementation)

A partitioned Writer that can still reach replicas but not the
coordinator has no mechanism to detect that it has been replaced. It
will continue sending items with the old epoch, which replicas will
reject (assuming the epoch bump has propagated), but the Writer
doesn't interpret epoch-rejection responses as "you've been replaced."

**In practice, the zombie is already functionally dead.** Once W or
more replicas reject its writes (stale epoch), `replicate()` can
never get W ACKs — every call hangs or times out. The user app's
writes are failing. The zombie just doesn't surface the reason
clearly.

**In the existing system,** the operator can kill a zombie primary
via Kubernetes (delete the pod). The Writer is a user process outside
the cluster — the operator can't kill it. So the Writer must
self-fence.

**Fix: epoch-stale error detection in the Writer library.**

This is not a protocol change — it's error handling in the Writer's
ACK processing path. When the Writer receives a `stale epoch`
rejection from a replica:

```rust
// In Writer's ACK reader / response handler:
match replica_response {
    Err(status) if status.code() == Code::FailedPrecondition
        && status.message().contains("stale epoch") => {
        stale_count += 1;
        if stale_count >= write_quorum {
            // W replicas have rejected us — we've been replaced
            self.self_fence();
            // Notify user app: WriterReplaced error on all
            // pending and future replicate() calls
        }
    }
    ...
}
```

The Writer transitions to a **fenced** state:
1. All pending `replicate()` calls resolve with `WriterReplaced` error
2. All future `replicate()` calls immediately return `WriterReplaced`
3. Writer attempts to reconnect to coordinator for confirmation
4. User app receives a clear, actionable error — not a mysterious
   timeout

**Severity: should-fix** but implementation is straightforward —
purely client-side error handling, no protocol changes, no
coordinator involvement.

---

- Voldemort client-side routing:
  `PipelineRoutedStore.java`, `PerformParallelPutRequests`
  [source](https://github.com/voldemort/voldemort/blob/master/src/java/voldemort/store/routed/PipelineRoutedStore.java)
- Voldemort design doc (archived):
  [project-voldemort.com/voldemort/design.html](https://web.archive.org/web/2018/http://www.project-voldemort.com/voldemort/design.html)
- Current kuberic replication:
  - `kuberic-core/src/handles.rs:162-186` — `replicate()` API
  - `kuberic-core/src/replicator/actor.rs:312-336` — actor data path
  - `kuberic-core/src/replicator/primary.rs:171-193` — `send_to_all()`
  - `kuberic-core/src/replicator/quorum.rs` — `QuorumTracker`
  - `kuberic-core/proto/kuberic.proto:134-160` — data plane proto
- Leaderless quorum survey:
  [docs/background/leaderless-quorum-replication.md](../../background/leaderless-quorum-replication.md)
