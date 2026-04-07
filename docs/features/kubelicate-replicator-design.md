# Kubelicate: SF-Style Stateful Replication on Kubernetes

High-level design for a Service Fabric–inspired stateful replication system
running on Kubernetes. The operator acts as the Failover Manager; user
application pods run an in-process replicator that reacts to operator signals.

**Implementation:** `kubelicate-core` crate (4600 LOC, 19 tests).

---

## Table of Contents

1. [Architecture Overview](#architecture-overview)
2. [Key Design Decisions](#key-design-decisions)
3. [Protocol: Failover](#protocol-failover)
4. [Protocol: Switchover](#protocol-switchover)
5. [Protocol: Create Partition](#protocol-create-partition)
6. [Access Status State Machine](#access-status-state-machine)
7. [Epoch Fencing](#epoch-fencing)
8. [Quorum Model](#quorum-model)
9. [Operator Design](#operator-design)
10. [User Application Model](#user-application-model)
11. [MVP Simplifications](#mvp-simplifications)
12. [Degenerate Configurations](#degenerate-configurations)
13. [Open Questions](#open-questions)
14. [Source Code Map](#source-code-map)

---

## Architecture Overview

```
┌──────────────────────────────────────────────────────────────────┐
│                      Kubernetes Cluster                          │
│                                                                  │
│  ┌──────────────────────┐                                        │
│  │ Kubelicate Operator  │   Watches: KubelicateSet CRD           │
│  │ (future crate)       │   Uses: PartitionDriver + GrpcReplica- │
│  │                      │   Handle to drive remote pods           │
│  └──────────┬───────────┘                                        │
│             │ gRPC (ReplicatorControl)                           │
│             ▼                                                    │
│  ┌───────────────────────────────────────────────────────┐       │
│  │               Replica Set (3 Pods)                    │       │
│  │                                                       │       │
│  │  ┌─────────────┐  ┌─────────────┐  ┌─────────────┐   │       │
│  │  │ Pod 1       │  │ Pod 2       │  │ Pod 3       │   │       │
│  │  │ PRIMARY     │  │ ACTIVE SEC  │  │ ACTIVE SEC  │   │       │
│  │  │             │  │             │  │             │   │       │
│  │  │ PodRuntime  │  │ PodRuntime  │  │ PodRuntime  │   │       │
│  │  │ ┌─────────┐ │  │ ┌─────────┐ │  │ ┌─────────┐ │   │       │
│  │  │ │Replicatr│◄├──├─┤Replicatr│ ├──├─┤Replicatr│ │   │       │
│  │  │ │Actor    │ │  │ │Actor    │ │  │ │Actor    │ │   │       │
│  │  │ └────┬────┘ │  │ └─────────┘ │  │ └─────────┘ │   │       │
│  │  │      │      │  │             │  │             │   │       │
│  │  │ User App    │  │ User App    │  │ User App    │   │       │
│  │  │ (events)    │  │ (standby)   │  │ (standby)   │   │       │
│  │  └─────────────┘  └─────────────┘  └─────────────┘   │       │
│  └───────────────────────────────────────────────────────┘       │
│                                                                  │
│  Services:                                                       │
│    {name}-rw  ──► Primary       (label: role=primary)            │
│    {name}-ro  ──► Secondaries   (label: role=secondary)          │
└──────────────────────────────────────────────────────────────────┘
```

---

## Key Design Decisions

| Decision | Choice | Rationale |
|---|---|---|
| **No StatefulSets** | Operator manages Pods directly | LSN-based primary selection, per-instance control |
| **Replicator is in-process** | Library linked into app, not sidecar | No coordination overhead, shared memory |
| **Operator-managed membership** | Operator pushes ReplicaSetConfig | Dual-config quorum, must-catch-up markers, idle/active gating |
| **Event-based APIs** | Both replicator and user use mpsc channels | Owned `&mut` state, no `Arc<Mutex<...>>` |
| **Dual-channel replicator** | Separate control (low-freq) and data (high-freq) channels | Control events don't block write path |
| **Atomic status reads** | PartitionState uses AtomicU8/AtomicI64 | Zero-cost read_status()/write_status() polling |
| **Fence-before-promote** | update_epoch to all secondaries BEFORE change_role(Primary) | Prevents zombie primary writes to unfenced nodes |
| **State provider as durability layer** | Persisted mode: `acknowledge()` gates quorum (SF default) | Strongest guarantee: replicate() returns only after quorum applied |
| **Primary self-fencing** | Liveness probe isolation check (CNPG-style) | Defense-in-depth for asymmetric partitions |
| **mTLS deferred** | Post-MVP; MVP assumes trusted cluster | Reduces initial complexity |
| **ReplicaHandle trait** | Driver works with any transport | Tests: in-process channels. Operator: gRPC client. |

---

## Protocol: Failover

Unplanned primary failure. Implemented in `PartitionDriver::failover()`.

```
1. Increment epoch (configuration_number++)
2. Fence ALL surviving secondaries: update_epoch(new_epoch)
   → Secondaries reject ops from old epoch
   → Set all status to ReconfigurationPending
3. Select new primary: replica with highest current_progress (LSN)
4. Promote: change_role(new_epoch, Primary) on winner
5. Reconfigure quorum: update_catch_up → wait_for_catch_up → update_current
6. Grant access: new primary → Granted, secondaries → NotPrimary
```

**Key invariant:** Step 2 (fence) happens BEFORE step 4 (promote). No window
for zombie primary to write to unfenced secondaries.

---

## Protocol: Switchover

Planned primary change. Implemented in `PartitionDriver::switchover()`.

```
1. Increment epoch
2. Revoke write status on old primary (reads still OK)
3. Fence all secondaries: update_epoch(new_epoch)
4. Revoke read status on old primary
5. Demote old primary: change_role(ActiveSecondary)
6. Promote target: change_role(Primary)
7. Reconfigure quorum
8. Grant access on new primary
```

**Staged revocation:** Write revoked first (step 2), reads later (step 4).
Minimizes client disruption.

---

## Protocol: Create Partition

Full SF-style build workflow. Implemented in `PartitionDriver::create_partition()`.

```
1. Open all replicators
2. Assign Primary role (replicator BEFORE status set)
3. Assign IdleSecondary to others + set epoch on SecondaryState
4. For each secondary:
   a. build_replica (primary → idle, copy stream)
   b. change_role(ActiveSecondary) — promote idle → active
   c. update_catch_up_configuration (add to quorum)
   d. wait_for_catch_up_quorum
   e. update_current_configuration (finalize)
5. Set access status: primary=Granted, secondaries=NotPrimary
```

Configuration grows **one secondary at a time** — each goes through the full
build → catch-up → finalize cycle.

---

## Access Status State Machine

**Normal operation:**

| State | ReadStatus | WriteStatus |
|---|---|---|
| Ready Primary | Granted | Granted (dynamic: NoWriteQuorum if quorum lost) |
| Secondary / Idle / None | NotPrimary | NotPrimary |

**During failover (S→P promotion on new primary):**

| Phase | Read | Write |
|---|---|---|
| GetLSN + Catchup | ReconfigPending | ReconfigPending |
| Catchup done | **Granted** | ReconfigPending |
| Activate done | Granted | **Granted** |

**During switchover (P→S on old primary):**

| Phase | Read | Write |
|---|---|---|
| Pre-catchup | Granted | **Granted** (double-catchup) |
| Write revoked | Granted | ReconfigPending |
| Read revoked | ReconfigPending | ReconfigPending |
| Role changed | NotPrimary | NotPrimary |

**Runtime owns status transitions** — the replicator only writes LSN values
to `PartitionState`. See `pod.rs::set_status_for_role()`.

---

## Epoch Fencing

```rust
struct Epoch { data_loss_number: i64, configuration_number: i64 }
```

- Lexicographic ordering (data_loss first)
- Incremented on every reconfiguration
- Secondaries reject operations with `epoch < current_epoch`
- On `update_epoch()`, secondaries **truncate uncommitted operations** to
  `committed_lsn` (prevents divergent state from zombie primary)
- Replicator tracks two LSNs: `committed_lsn` (safe) and `received_lsn`
  (may be ahead if primary sent uncommitted ops)

---

## Quorum Model

```
Write Quorum = ⌊N/2⌋ + 1  (primary counts toward quorum)

3 replicas → quorum=2 → tolerates 1 failure
5 replicas → quorum=3 → tolerates 2 failures
```

**Dual-config quorum during reconfiguration:** Writes must be ACKed by
quorum from BOTH previous and current configurations. A replica in both
configs counts toward both with one ACK. See `QuorumTracker`.

---

## Operator Design

**CRD:** `KubelicateSet` with `spec.replicas`, `spec.minReplicas`,
`spec.failoverDelay`, `spec.switchoverDelay`.

**Status fields:** `epoch`, `currentPrimary`, `targetPrimary`, `phase`,
`reconfigurationPhase`, `currentConfiguration`, `previousConfiguration`.

**Reconciliation:** Uses `PartitionDriver` with `GrpcReplicaHandle`
(same driver as tests, different transport). CRD status is the durable
state machine — write-ahead pattern (CRD write before gRPC calls).

**All gRPC calls have timeouts** (default 30s). One reconfiguration at a
time. Phases are idempotent (safe to re-execute after operator crash).

---

## User Application Model

Event-based — user receives `ServiceEvent` on an mpsc channel. Events fall
into two categories: **lifecycle events** (SF's `IStatefulServiceReplica`)
and **state provider callbacks** (SF's `IStateProvider`).

### Lifecycle Events

```
ServiceEvent::Open { ctx, reply }         — store handles (Partition, StateReplicator, streams)
ServiceEvent::ChangeRole { role, reply }  — start/stop work based on role
ServiceEvent::Close { reply }             — drain and cleanup
ServiceEvent::Abort                       — emergency stop
```

### State Provider Callbacks

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

### Event Enums and Channels

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

**User event loop with prioritized select:**

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
                    // This may take a while — but won't block lifecycle events
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

### When Each Event is Delivered

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

### Secondary Data Delivery (Pull Model)

Aligned with the SF `IStateReplicator` pattern (verified against the
[kvmap sample](https://github.com/youyuanwu/service-fabric-apps-rs/blob/main/crates/samples/kvmap/src/kvmap.rs)).
The user pulls operations from streams and calls `acknowledge()` to release.

#### Interfaces

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

#### ServiceContext

```rust
pub struct ServiceContext {
    pub partition: Arc<PartitionHandle>,
    pub replicator: StateReplicatorHandle,          // primary: replicate()
    pub copy_stream: Option<OperationStream>,       // None on primary
    pub replication_stream: Option<OperationStream>, // None on primary
    pub token: CancellationToken,
}
```

#### Data Flow

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

#### Stream Usage by Role (from SF kvmap sample)

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

#### StateProvider Callbacks (How Copy Works)

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

#### Boundary Between Copy and Replication

- `up_to_lsn` = primary's committed LSN at build start
- Copy: `[0, up_to_lsn]` — full state
- Replication: `(up_to_lsn, ∞)` — incremental ops
- Replicator buffers ops > `up_to_lsn` during copy, delivers after

#### Backpressure

**Persisted mode:** Slow `acknowledge()` directly slows primary writes
(quorum waits for ack). This is by design — it prevents the primary from
outrunning the secondaries. If a secondary is too slow, the operator should
replace it.

**Volatile mode:** Slow `acknowledge()` → ops queue in replicator → no
quorum impact → memory grows → configurable limit → health warning or
rebuild.

---

## MVP Simplifications

| Full Design | MVP (implemented) |
|---|---|
| Replicator WAL on disk | No WAL — replicator queues in memory, user `acknowledge()` releases |
| Incremental catchup via WAL replay | Full rebuild via copy stream |
| build_replica copies state + replays WAL | Full state copy only (build_replica is no-op in MVP) |
| Copy stream delivers full state | Copy stream not yet wired (no-op build) |
| `acknowledge()` gates quorum (persisted mode) | MVP: replicator auto-ACKs on receive (volatile mode) |
| `must_catch_up` enforced in catchup | MVP: QuorumTracker doesn't enforce must_catch_up independently |
| Build completes on copy+repl ACKed | MVP: build_replica is a no-op |
| mTLS on all gRPC | Deferred — assumes trusted cluster |
| ReportFault rate limiting | Operator-side, basic |

### Known Gaps vs SF (to address post-MVP)

1. **Acknowledge-gated quorum.** Current code ACKs to primary on replicator
   receive, not on user `acknowledge()`. To implement persisted mode, the
   secondary replicator must hold the ACK until the user pulls the operation
   from the stream and calls `acknowledge()`. This requires wiring the
   `OperationStream` into the `SecondaryReceiver`'s ACK path.

2. **must_catch_up enforcement.** `QuorumTracker` counts all ACKs equally.
   For `wait_for_catch_up_quorum(Write)`, it should additionally verify that
   the `must_catch_up` replica (the primary candidate) is fully caught up —
   not just that a quorum is caught up.

3. **Build completion condition.** SF build completes when the secondary ACKs
   both the last copy op AND the last replication op that existed at the copy
   boundary moment. This handles concurrent writes during build. Our build
   is a no-op — when implemented, it needs this two-phase completion check.

---

## Degenerate Configurations

**replicas=1:** No replication, no failover. Pod restart = resume as primary.
Development/testing only.

**replicas=2:** Write quorum=2 (both must ACK). Zero write fault tolerance.
Single failure → NoWriteQuorum. Failover is safe (survivor has all data).

**Minimum recommended:** `replicas >= 3`. Pod anti-affinity across nodes
(ensures failure independence for quorum model).

---

## Open Questions

1. **Multi-partition support** — multiple independent replica sets per
   KubelicateSet.
2. **Pod identity and PVC binding** — how to bind recreated pods to correct
   PVCs.
3. **Replication transport** — gRPC vs QUIC vs direct TCP for data plane.

---

## Source Code Map

```
kubelicate-core/
├── proto/kubelicate.proto           # gRPC: ReplicatorControl (12 RPCs) + ReplicatorData
├── src/
│   ├── types.rs                     # Epoch, Role, AccessStatus, ReplicaInfo, ReplicaSetConfig
│   ├── error.rs                     # KubelicateError enum (NotPrimary, NoWriteQuorum, etc.)
│   ├── events.rs                    # ReplicatorControlEvent, ReplicateRequest, ServiceEvent
│   ├── handles.rs                   # PartitionState (atomics), PartitionHandle, StateReplicatorHandle
│   ├── noop.rs                      # NoopReplicator actor (testing)
│   ├── runtime.rs                   # KubelicateRuntime (lower-level harness)
│   ├── pod.rs                       # PodRuntime (full pod: actor + gRPC + user events)
│   ├── driver.rs                    # PartitionDriver + ReplicaHandle trait + InProcessReplicaHandle
│   ├── replicator/
│   │   ├── quorum.rs                # QuorumTracker (single + dual-config)
│   │   ├── actor.rs                 # WalReplicatorActor (control + data event loop)
│   │   ├── primary.rs               # PrimarySender (gRPC streams to secondaries)
│   │   └── secondary.rs             # SecondaryReceiver (gRPC server, epoch validation)
│   └── grpc/
│       ├── convert.rs               # Proto ↔ domain type conversions
│       ├── server.rs                # ControlServer (ReplicatorControl gRPC impl)
│       └── handle.rs                # GrpcReplicaHandle (remote ReplicaHandle for operator)
```

**Tests (19):**
- `noop.rs` — 3 tests: lifecycle, replicate handle, not-primary rejection
- `runtime.rs` — 3 tests: full lifecycle, replicate-before-promote, abort
- `replicator/quorum.rs` — 5 tests: single/three/dual-config, out-of-order, fail-all
- `driver.rs` — 5 tests: create+replicate+delete, single, failover, switchover, epoch truncation
- `grpc/e2e_tests.rs` — 2 tests: full gRPC create+replicate+delete, gRPC failover
- `pod.rs` — 1 test: user lifecycle (open→promote→replicate→demote→close)

**Background references:**
- `docs/background/service-fabric-stateful-failover.md` — SF architecture study
- `docs/background/cloudnative-pg-architecture.md` — CNPG architecture study
- `build/service-fabric-rs/` — SF Rust bindings (reference)
- `build/service-fabric/` — SF C++ source (reference)
- `build/cloudnative-pg/` — CNPG Go source (reference)
