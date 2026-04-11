# Kubelicate: User Application Model

Event-based API for building stateful services on Kubelicate. The user
creates the replicator at Open time, keeps `ServiceContext` handles,
and interacts with the replicator via `StateReplicatorHandle`.

> Part of the [Kubelicate Design](../kubelicate-replicator-design.md).
> Separation design: [Runtime–Replicator Separation](implemented/runtime-replicator-separation.md).

---

## Overview

The runtime delivers `LifecycleEvent` on an mpsc channel. The critical
event is **Open**, where the user creates the replicator and returns a
`ReplicatorHandle` to the runtime. The user keeps `ServiceContext` with
the write handle, partition, streams, and state provider channel.

---

## Open Flow (User Creates Replicator)

```
Runtime ──── LifecycleEvent::Open { ctx: OpenContext } ────► User
                                                               │
                                                    1. WalReplicator::create(...)
                                                    2. Keep ServiceContext
                                                    3. Return ReplicatorHandle
                                                               │
Runtime ◄──── Ok(ReplicatorHandle) ────────────────────────────┘
```

The user chooses which replicator implementation to use. Currently
`WalReplicator` is the only implementation (WAL-based persisted quorum).

---

## Lifecycle Events

```rust
pub enum LifecycleEvent {
    Open {
        ctx: OpenContext,
        reply: oneshot::Sender<Result<ReplicatorHandle>>,
    },
    ChangeRole {
        new_role: Role,
        reply: oneshot::Sender<Result<String>>,
    },
    Close { reply: oneshot::Sender<Result<()>> },
    Abort,
}
```

| Event | When | What to do |
|-------|------|------------|
| `Open` | Pod startup | Create replicator, keep handles, return `ReplicatorHandle` |
| `ChangeRole` | Role change | Start/stop background work based on role |
| `Close` | Graceful shutdown | Drain in-flight work, checkpoint, reply |
| `Abort` | Emergency shutdown | Release resources, no reply |

---

## OpenContext and ServiceContext

```rust
/// Provided by the runtime at Open.
pub struct OpenContext {
    pub replica_id: ReplicaId,
    pub open_mode: OpenMode,
    pub data_bind: String,
    pub token: CancellationToken,
    pub fault_tx: mpsc::Sender<FaultType>,
}

/// Produced by WalReplicator::create(). User keeps these.
pub struct ServiceContext {
    pub replicator: StateReplicatorHandle,
    pub partition: Arc<PartitionHandle>,
    pub copy_stream: Option<OperationStream>,
    pub replication_stream: Option<OperationStream>,
    pub state_provider_rx: mpsc::Receiver<StateProviderEvent>,
}
```

---

## State Provider Callbacks

Delivered on `ServiceContext.state_provider_rx`. These are sent by the
replicator (not the runtime) when it needs cooperation from the user's
state. The user handles them in a `tokio::select!` alongside lifecycle.

```rust
pub enum StateProviderEvent {
    UpdateEpoch { epoch, previous_epoch_last_lsn, reply },
    GetLastCommittedLsn { reply },
    GetCopyContext { reply },
    GetCopyState { up_to_lsn, copy_context, reply },
    OnDataLoss { reply },
}
```

| Event | Sender | When | Role |
|-------|--------|------|------|
| `UpdateEpoch` | Replicator | Reconfiguration | Secondary |
| `GetLastCommittedLsn` | Replicator | Build / catchup | Secondary |
| `GetCopyContext` | Replicator | build_replica | Idle (new) |
| `GetCopyState` | Replicator | build_replica | Primary |
| `OnDataLoss` | Replicator | Quorum loss | New Primary |

---

## User Event Loop

```rust
async fn run_service(
    mut lifecycle_rx: mpsc::Receiver<LifecycleEvent>,
    state: SharedState,
    client_bind: String,
) {
    let mut replicator = None;
    let mut state_provider_rx = None;

    loop {
        tokio::select! {
            biased; // Lifecycle takes priority

            Some(event) = lifecycle_rx.recv() => match event {
                LifecycleEvent::Open { ctx, reply } => {
                    // Create replicator, keep handles
                    let (handle, svc_ctx) = WalReplicator::create(
                        ctx.replica_id, &ctx.data_bind, ctx.fault_tx.clone(),
                    ).await.unwrap();
                    replicator = Some(svc_ctx.replicator);
                    state_provider_rx = Some(svc_ctx.state_provider_rx);
                    // ... store partition, streams, token ...
                    let _ = reply.send(Ok(handle));
                }
                LifecycleEvent::ChangeRole { new_role, reply } => {
                    // start/stop work
                    let _ = reply.send(Ok(String::new()));
                }
                LifecycleEvent::Close { reply } => {
                    let _ = reply.send(Ok(()));
                    break;
                }
                LifecycleEvent::Abort => break,
            },

            Some(event) = async {
                match state_provider_rx.as_mut() {
                    Some(rx) => rx.recv().await,
                    None => std::future::pending().await,
                }
            } => match event {
                StateProviderEvent::UpdateEpoch { previous_epoch_last_lsn, reply, .. } => {
                    rollback_uncommitted(previous_epoch_last_lsn);
                    let _ = reply.send(Ok(()));
                }
                // ... handle other state provider events ...
                _ => {}
            },

            else => break,
        }
    }
}
```

**Key difference from old API:** The user no longer receives handles
passively — they create the replicator in the Open handler and choose
which implementation to use.

---

## PodRuntimeBundle

```rust
pub struct PodRuntimeBundle {
    pub runtime: PodRuntime,
    pub lifecycle_rx: mpsc::Receiver<LifecycleEvent>,
    pub control_address: String,
}
```

`data_address` is no longer in the bundle — it comes from the replicator
created at Open time (`ReplicatorHandle::data_address()`).

---

## ChangeRole Ordering

| Direction | Transitions | Order |
|-----------|-------------|-------|
| **Promotion** | None→Primary, Idle→Active, *→Primary | 1. Replicator  2. Status  3. User |
| **Demotion** | P→S, Active→None, *→None | 1. Status  2. User  3. Replicator |

On promotion, the replicator is ready before the user starts writing.
On demotion, writes are fenced before the user is notified.

---

## Secondary Data Delivery (Pull Model)

Aligned with the SF `IStateReplicator` pattern (verified against the
[kvmap sample](https://github.com/youyuanwu/service-fabric-apps-rs/blob/main/crates/samples/kvmap/src/kvmap.rs)).
The user pulls operations from streams and calls `acknowledge()` to release.

### Interfaces

```rust
pub struct Operation {
    pub lsn: Lsn,
    pub data: Bytes,
    ack_tx: Option<oneshot::Sender<()>>,
}

impl Operation {
    /// Mandatory for persisted replicators — blocks subsequent ops until called.
    pub fn acknowledge(mut self) {
        if let Some(tx) = self.ack_tx.take() { let _ = tx.send(()); }
    }
}

pub struct OperationStream {
    rx: mpsc::Receiver<Operation>,
}

impl OperationStream {
    /// Returns None when stream ends.
    pub async fn get_operation(&mut self) -> Option<Operation> {
        self.rx.recv().await
    }
}
```

### Data Flow

`Acknowledge()` semantics depend on the replicator mode (matching SF):

**Persisted mode** (`require_service_ack = true`, default):
```
Primary                    Secondary Replicator           Secondary User
   │                            │                              │
   │── ReplicationItem ────────►│                              │
   │                            │  1. Validate epoch           │
   │                            │  2. Queue in replicator      │
   │                            │  3. User pulls               │
   │                            │◄── get_operation() ─────────│
   │                            │──── Operation(lsn, data) ───►│
   │                            │                        4. apply + persist
   │                            │◄──── acknowledge() ─────────│
   │                            │  5. ACK to primary           │
   │◄── ACK ───────────────────│   (quorum gated on ack)      │
   │  replicate() completes     │  6. Release from queue       │
```

**Volatile mode** (`require_service_ack = false`):
```
Primary                    Secondary Replicator           Secondary User
   │                            │                              │
   │── ReplicationItem ────────►│                              │
   │                            │  1. Validate epoch           │
   │                            │  2. Queue + auto-ACK         │
   │◄── ACK ───────────────────│   (quorum on receive)        │
   │  replicate() completes     │                              │
   │                            │  3. User pulls (async)       │
   │                            │◄── get_operation() ─────────│
   │                            │──── Operation(lsn, data) ───►│
   │                            │                        4. apply
   │                            │◄──── acknowledge() ─────────│
   │                            │  5. Release from queue       │
```

**Our default is persisted mode** — `acknowledge()` gates quorum. This
gives the strongest durability guarantee: primary's `replicate()` returns
only after a quorum of secondaries have applied + acknowledged.

### Stream Usage by Role

**`ChangeRole(IdleSecondary)`** — drain copy stream:

```rust
if let Some(copy) = copy_stream.take() {
    tokio::spawn(drain_stream(state.clone(), copy, token.clone(), "copy"));
}
```

**`ChangeRole(ActiveSecondary)`** — drain replication stream:

```rust
if let Some(repl) = replication_stream.take() {
    tokio::spawn(drain_stream(state.clone(), repl, token.clone(), "replication"));
}
```

**`ChangeRole(Primary)`** — user writes via `replicate()`:

```rust
let data = serde_json::to_vec(&("key", "value")).unwrap();
let lsn = replicator.replicate(Bytes::from(data), token).await?;
```

### Copy Protocol (StateProvider Callbacks)

The copy stream is produced by a bidirectional exchange between the
primary's and secondary's state provider callbacks:

```
Secondary (idle)              System                 Primary
      │                         │                      │
      │◄── GetCopyContext ─────│                      │
      │──── stream(my_lsn) ───►│                      │
      │                         │── GetCopyState ─────►│
      │                         │   (up_to_lsn,        │
      │                         │    context_stream)    │
      │                         │◄── stream(state) ────│
      │◄── copy_stream ───────│                      │
      │    (apply + ack)        │                      │
```

**`GetCopyContext`** (secondary): returns a stream with the secondary's
current LSN. Primary uses this to know what the secondary already has.

**`GetCopyState`** (primary): receives `up_to_lsn` + copy context stream.
Reads the secondary's LSN from context, produces only the missing state.

Example from the KV app:

```rust
// GetCopyContext (secondary) — send current LSN
StateProviderEvent::GetCopyContext { reply } => {
    let lsn = state.read().await.last_applied_lsn;
    let (tx, stream) = OperationStream::channel(1);
    let _ = tx.send(Operation::new(0, Bytes::from(lsn.to_string()), None)).await;
    drop(tx);
    let _ = reply.send(Ok(stream));
}

// GetCopyState (primary) — read context, produce state
StateProviderEvent::GetCopyState { up_to_lsn, mut copy_context, reply } => {
    let peer_lsn = if let Some(op) = copy_context.get_operation().await {
        String::from_utf8_lossy(&op.data).parse::<i64>().unwrap_or(0)
    } else { 0 };

    // Snapshot under read lock, then send without lock
    let snapshot = { /* collect state */ };
    let (tx, stream) = OperationStream::channel(64);
    let _ = reply.send(Ok(stream));
    for (k, v) in snapshot { /* send ops */ }
    drop(tx);
}
```

### Boundary Between Copy and Replication

- `up_to_lsn` = primary's committed LSN at build start
- Copy: `[0, up_to_lsn]` — full state
- Replication: `(up_to_lsn, ∞)` — incremental ops
- Replicator buffers ops > `up_to_lsn` during copy, delivers after

### Backpressure

**Persisted mode:** Slow `acknowledge()` directly slows primary writes
(quorum waits for ack). This is by design — it prevents the primary from
outrunning the secondaries. If a secondary is too slow, the operator should
replace it.

**Volatile mode:** Slow `acknowledge()` → ops queue in replicator → no
quorum impact → memory grows → configurable limit → health warning or
rebuild.
