# Kubelicate: User Application Model

Event-based API for building stateful services on Kubelicate. The user
receives events on mpsc channels, processes them, and interacts with
the replicator via `StateReplicatorHandle`.

> Part of the [Kubelicate Design](../kubelicate-replicator-design.md).

---

## Overview

User receives `ServiceEvent` on an mpsc channel. Events fall into two
categories: **lifecycle events** (SF's `IStatefulServiceReplica`) and
**state provider callbacks** (SF's `IStateProvider`).

---

## Lifecycle Events

```
ServiceEvent::Open { ctx, reply }         — store handles (Partition, StateReplicator, streams)
ServiceEvent::ChangeRole { role, reply }  — start/stop work based on role
ServiceEvent::Close { reply }             — drain and cleanup
ServiceEvent::Abort                       — emergency stop
```

---

## State Provider Callbacks

These are called by the replicator when it needs information about or
cooperation from the user's state. They mirror SF's `IStateProvider`.

```
ServiceEvent::GetLastCommittedLsn { reply }
    — Replicator asks: "what's your last durably applied LSN?"
    — Called during build and catchup to determine progress.
    — Reply: Result<Lsn>

ServiceEvent::GetCopyContext { reply }
    — Called on a new secondary to send context to the primary.
    — The secondary tells the primary what state it already has
      (e.g., a checkpoint LSN, or empty for a brand-new replica).
    — Reply: Result<OperationStream> (stream of context data)
    — For empty replicas, reply with an empty stream.

ServiceEvent::GetCopyState { up_to_lsn, copy_context, reply }
    — Called on the primary during build_replica.
    — The primary produces the full state up to `up_to_lsn`,
      using the secondary's copy_context to send only what's needed.
    — Reply: Result<OperationStream> (stream of state data)
    — The replicator delivers this to the secondary's copy_stream.

ServiceEvent::OnDataLoss { reply }
    — Called when write quorum was lost and data loss may have occurred.
    — The user can restore from backup, accept the loss, or reconcile.
    — Reply: Result<bool> (true if state was changed → triggers rebuild)
```

---

## Event Enums and Channels

Events are delivered on **two separate channels**, allowing the user to
prioritize lifecycle events over state provider callbacks:

- **`lifecycle_rx`** — `mpsc::Receiver<LifecycleEvent>` — rare, must be
  handled immediately (ChangeRole, Close)
- **`state_provider_rx`** — `mpsc::Receiver<StateProviderEvent>` — may be
  slow (GetCopyState produces a full state snapshot)

```rust
/// Lifecycle events (SF's IStatefulServiceReplica).
/// Delivered on all roles. Handle immediately.
pub enum LifecycleEvent {
    Open {
        ctx: ServiceContext,
        reply: oneshot::Sender<Result<()>>,
    },
    ChangeRole {
        new_role: Role,
        reply: oneshot::Sender<Result<String>>,
    },
    Close {
        reply: oneshot::Sender<Result<()>>,
    },
    Abort,
}

/// State provider callbacks (SF's IStateProvider).
/// Role-specific. May involve heavy work (copy state production).
pub enum StateProviderEvent {
    /// Epoch changed (secondaries only).
    /// Primary gets epoch via ChangeRole, not UpdateEpoch.
    UpdateEpoch {
        epoch: Epoch,
        previous_epoch_last_lsn: Lsn,
        reply: oneshot::Sender<Result<()>>,
    },

    /// "What's your last applied LSN?" (secondary, during build/catchup)
    GetLastCommittedLsn {
        reply: oneshot::Sender<Result<Lsn>>,
    },

    /// "What state do you already have?" (new idle secondary, during build)
    GetCopyContext {
        reply: oneshot::Sender<Result<OperationStream>>,
    },

    /// "Produce state for this secondary" (primary, during build_replica)
    GetCopyState {
        up_to_lsn: Lsn,
        copy_context: OperationStream,
        reply: oneshot::Sender<Result<OperationStream>>,
    },

    /// "Quorum was lost, data loss possible" (new primary after quorum loss)
    OnDataLoss {
        reply: oneshot::Sender<Result<bool>>,
    },
}
```

**ServiceContext (provided at Open):**

```rust
pub struct ServiceContext {
    pub partition: Arc<PartitionHandle>,
    pub replicator: StateReplicatorHandle,
    pub copy_stream: Option<OperationStream>,
    pub replication_stream: Option<OperationStream>,
    pub token: CancellationToken,
}
```

**PodRuntimeBundle provides both channels:**

```rust
pub struct PodRuntimeBundle {
    pub runtime: PodRuntime,
    pub lifecycle_rx: mpsc::Receiver<LifecycleEvent>,
    pub state_provider_rx: mpsc::Receiver<StateProviderEvent>,
    pub control_address: String,
    pub data_address: String,
}
```

---

## User Event Loop (Prioritized Select)

```rust
async fn run_service(
    mut lifecycle_rx: mpsc::Receiver<LifecycleEvent>,
    mut state_provider_rx: mpsc::Receiver<StateProviderEvent>,
) {
    let mut ctx = None;

    loop {
        tokio::select! {
            biased; // Lifecycle takes priority

            Some(event) = lifecycle_rx.recv() => match event {
                LifecycleEvent::Open { ctx: c, reply } => {
                    ctx = Some(c);
                    let _ = reply.send(Ok(()));
                }
                LifecycleEvent::ChangeRole { new_role, reply } => {
                    // start/stop background work based on role
                    let _ = reply.send(Ok(String::new()));
                }
                LifecycleEvent::Close { reply } => {
                    let _ = reply.send(Ok(()));
                    break;
                }
                LifecycleEvent::Abort => break,
            },

            Some(event) = state_provider_rx.recv() => match event {
                StateProviderEvent::GetLastCommittedLsn { reply } => {
                    let _ = reply.send(Ok(my_last_lsn));
                }
                StateProviderEvent::GetCopyState { up_to_lsn, copy_context, reply } => {
                    let stream = produce_copy_state(up_to_lsn, copy_context);
                    let _ = reply.send(Ok(stream));
                }
                StateProviderEvent::UpdateEpoch { previous_epoch_last_lsn, reply, .. } => {
                    truncate_stale_ops(previous_epoch_last_lsn);
                    let _ = reply.send(Ok(()));
                }
                _ => {}
            },

            else => break,
        }
    }
}
```

**Why two channels:**

| Concern | Lifecycle channel | StateProvider channel |
|---|---|---|
| Frequency | Rare (startup, role change, shutdown) | During build/catchup/failover |
| Latency requirement | Immediate (blocks reconfiguration) | Can take time (copy state) |
| Priority | **High** — ChangeRole/Close must not be delayed | Normal |
| Blocking risk | Never blocked by state provider work | Can be slow without affecting lifecycle |

---

## When Each Event is Delivered

| Event | Group | Called by | When | Applicable roles |
|---|---|---|---|---|
| `Open` | Lifecycle | Runtime | Pod startup | All |
| `ChangeRole` | Lifecycle | Runtime | Role change | All |
| `Close` | Lifecycle | Runtime | Graceful shutdown | All |
| `Abort` | Lifecycle | Runtime | Emergency shutdown | All |
| `UpdateEpoch` | StateProvider | Replicator | Reconfiguration | Secondary, Idle |
| `GetLastCommittedLsn` | StateProvider | Replicator | Build / catchup | Secondary, Idle |
| `GetCopyContext` | StateProvider | Replicator | build_replica | Idle (new) |
| `GetCopyState` | StateProvider | Replicator | build_replica | Primary |
| `OnDataLoss` | StateProvider | Replicator | Quorum loss | New Primary |

**Note on `UpdateEpoch`:** Called on secondaries that stay in the same role
but receive a new epoch (e.g., a different replica was promoted). The user
receives `previous_epoch_last_lsn` — operations with LSN above this from
the old epoch may be stale (uncommitted zombie primary writes).

**Ordering guarantee:** On promotion (S→P), the replicator is set up before
the user is notified. On demotion (P→S), the user stops before the replicator
tears down.

**Writes (primary):** `ctx.replicator.replicate(data, token)` — goes through
the data channel to the quorum-replicating actor. Fast-path `write_status()`
check before entering the channel.

**Status polling:** `ctx.partition.read_status()` / `write_status()` are
atomic reads (~1ns). No events for status changes — user polls in their
work loop.

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

### Stream Usage by Role (from SF kvmap sample)

**`ChangeRole(IdleSecondary)`** — drain copy stream:

```rust
if let Some(mut copy) = ctx.copy_stream.take() {
    tokio::spawn(async move {
        while let Some(op) = copy.get_operation().await {
            let kv: (String, String) = serde_json::from_slice(&op.data).unwrap();
            state.insert(kv.0, kv.1);
            op.acknowledge();
        }
        info!("copy stream drained");
    });
}
```

**`ChangeRole(ActiveSecondary)`** — drain replication stream:

```rust
if let Some(mut repl) = ctx.replication_stream.take() {
    tokio::spawn(async move {
        while let Some(op) = repl.get_operation().await {
            let kv: (String, String) = serde_json::from_slice(&op.data).unwrap();
            state.insert(kv.0, kv.1);
            op.acknowledge();
            info!(lsn = op.lsn, "applied repl op");
        }
    });
}
```

**`ChangeRole(Primary)`** — user writes via `replicate()`:

```rust
let data = serde_json::to_vec(&("key", "value")).unwrap();
let lsn = ctx.replicator.replicate(Bytes::from(data), token).await?;
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

Example from the SF kvmap sample:

```rust
// GetCopyContext (secondary) — send current LSN
StateProviderEvent::GetCopyContext { reply } => {
    let (tx, rx) = mpsc::channel(1);
    tx.send(Operation { lsn: 0, data: Bytes::from(last_lsn.to_string()), ack_tx: None }).await;
    drop(tx);
    reply.send(Ok(OperationStream { rx }));
}

// GetCopyState (primary) — read context, produce state
StateProviderEvent::GetCopyState { up_to_lsn, mut copy_context, reply } => {
    let peer_lsn = if let Some(op) = copy_context.get_operation().await {
        String::from_utf8_lossy(&op.data).parse::<i64>().unwrap_or(0)
    } else { 0 };

    let (tx, rx) = mpsc::channel(16);
    if peer_lsn < last_lsn {
        for (k, v) in &state {
            let data = serde_json::to_vec(&(k, v)).unwrap();
            tx.send(Operation { lsn: last_lsn, data: Bytes::from(data), ack_tx: None }).await;
        }
    }
    drop(tx);
    reply.send(Ok(OperationStream { rx }));
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
