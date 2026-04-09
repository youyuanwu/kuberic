# Kubelicate: Replication Protocols

Protocols implemented by `PartitionDriver` (operator-side orchestration)
and `WalReplicatorActor` (pod-side replication engine).

> Part of the [Kubelicate Design](../kubelicate-replicator-design.md).

---

## Protocol: Failover

Unplanned primary failure. Implemented in `PartitionDriver::failover()`.

```
1. Enforce failover delay (if spec.failoverDelay > 0, K8s-specific):
   - Record failingSinceTimestamp
   - Requeue until delay elapsed or primary recovers
   - Default: 0 (immediate, matching SF behavior)
2. Increment epoch (configuration_number++)
3. Fence ALL surviving secondaries: update_epoch(new_epoch)
   → Secondaries reject ops from old epoch
   → Set all status to ReconfigurationPending
4. Select new primary: replica with highest current_progress (LSN)
   - Exclude crash-looping and unreachable replicas
   - Matches SF FM's CompareForPrimary() logic
5. Check quorum availability:
   a. If write quorum available (≥ ⌊N/2⌋+1 survivors):
      Normal failover — no data loss
   b. If write quorum NOT available:
      Wait spec.quorumLossWaitDuration (default 60s)
      for replicas to recover (SF QuorumLossWaitDuration)
      If replicas recover → normal failover (no data loss)
      If timeout expires → data loss protocol:
        Increment epoch.data_loss_number
        Promote best available replica
        Call on_data_loss() on new primary
        Rebuild all secondaries from new primary
6. Promote: change_role(new_epoch, Primary) on winner
7. Reconfigure quorum: update_catch_up → wait_for_catch_up → update_current
8. Grant access: new primary → Granted, secondaries → NotPrimary
9. Clean up old primary: close/delete pod, create replacement secondary
```

**Key invariant:** Step 3 (fence) happens BEFORE step 6 (promote). No window
for zombie primary to write to unfenced secondaries.

**SF alignment:** Step 5b matches SF's `ReconfigurationTask` behavior:
wait `QuorumLossWaitDuration` before declaring data loss. The user's
`on_data_loss()` handler is the decision point. This is a middle ground —
SF waits but eventually proceeds (unlike CNPG which blocks indefinitely).

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

## Protocol: Scale-Up (Add Replica)

Add a new replica to a running partition. Implemented in
`PartitionDriver::add_replica()`.

```
1. Operator creates new Pod + waits for Ready
2. Operator creates GrpcReplicaHandle for the new pod
3. driver.add_replica(handle):
   a. Open replicator on new pod
   b. change_role(IdleSecondary) + set epoch
   c. build_replica (primary → idle, copy stream)
   d. change_role(ActiveSecondary) — promote idle → active
   e. update_catch_up_configuration (add to quorum)
   f. wait_for_catch_up_quorum
   g. update_current_configuration (finalize)
4. Operator updates CRD status
```

Same as create_partition steps 4a–4e, but for a single replica joining
an already-running partition.

---

## Protocol: Restart Secondary

Replace a failed secondary. Implemented in `PartitionDriver::restart_secondary()`.

```
1. Close old secondary handle
2. remove_secondary (quorum reconfiguration)
3. Operator deletes old Pod + creates new Pod
4. Wait for Ready
5. add_replica (full build from primary)
```

Delegates to `add_replica` after cleanup.

---

## Protocol: Scale-Down (Remove Secondary)

Remove a replica from the partition. Implemented in
`PartitionDriver::remove_secondary()`.

Design follows SF's config-first approach (remove from quorum before closing):

```
1. Verify replica_count > min_replicas (safety)
2. update_catch_up_configuration (new config WITHOUT the replica)
3. wait_for_catch_up_quorum(Write)
4. update_current_configuration (finalize)
5. change_role(None) on removed replica
6. close removed replica
7. Remove handle from driver
```

**Safety:**
- Cannot scale below `spec.minReplicas`
- Config update happens BEFORE close — write quorum is maintained
- If primary is targeted, operator must switchover first

---

## Copy-to-Replication Stream Transition

How a new secondary goes from receiving a full state copy to receiving
live replication — the complete orchestration across all components.

### Components Involved

```
Operator (PartitionDriver)
  │ control plane RPCs
  ▼
Primary Pod (PodRuntime + WalReplicatorActor + PrimarySender)
  │ data plane gRPC
  ▼
Secondary Pod (SecondaryReceiver + user app)
  │ two mpsc channels
  ▼
User code (drains copy_stream, then replication_stream)
```

### Stream Setup (Pod Build Time)

Both streams are created when the secondary pod starts, **before** any
copy or replication begins:

```rust
// pod.rs — PodRuntimeBuilder::build()
let (repl_op_tx, repl_op_rx) = mpsc::channel(256);
let replication_stream = OperationStream::new(repl_op_rx);

let (copy_op_tx, copy_op_rx) = mpsc::channel(256);
let copy_stream = OperationStream::new(copy_op_rx);

// SecondaryReceiver gets the senders (repl_op_tx, copy_op_tx)
// User gets the receivers via ServiceContext at Open time
```

The `SecondaryReceiver` (data plane gRPC server) holds `repl_op_tx` and
`copy_op_tx`. When RPCs arrive, data flows into the appropriate channel.
The user holds the receiving ends via `ServiceContext.copy_stream` and
`ServiceContext.replication_stream`.

### Full Orchestration Sequence

```
Operator                    Primary                     Secondary             User
   │                           │                           │                    │
   │ 1. open(secondary)        │                           │                    │
   │──────────────────────────────────────────────────────►│                    │
   │                           │                           │──Open event──────►│
   │                           │                           │  (gives user       │
   │                           │                           │   copy_stream +    │
   │                           │                           │   replication_     │
   │                           │                           │   stream handles)  │
   │                           │                           │                    │
   │ 2. change_role(Idle)      │                           │                    │
   │──────────────────────────────────────────────────────►│                    │
   │                           │                           │──ChangeRole(Idle)─►│
   │                           │                           │  User starts       │
   │                           │                           │  draining          │
   │                           │                           │  copy_stream       │
   │                           │                           │                    │
   │ 3. build_replica ─────────────────────────────────►│  │                    │
   │    (RPC to PRIMARY, blocks until copy done)        │  │                    │
   │                           │                        │  │                    │
   │                           │ 3a. GetCopyContext RPC ►│  │                    │
   │                           │◄── context stream ─────│  │──GetCopyContext──►│
   │                           │                           │◄── user's LSN ────│
   │                           │                           │                    │
   │                           │ 3b. GetCopyState          │                    │
   │                           │   (asks own StateProvider) │                    │
   │                           │   produces state snapshot  │                    │
   │                           │                           │                    │
   │                           │ 3c. CopyStream RPC ──────►│                    │
   │                           │     streams all state     │                    │
   │                           │     items to secondary    │──items flow into──►│
   │                           │                           │  copy_op_tx →      │
   │                           │                           │  copy_stream →     │
   │                           │                           │  user applies      │
   │                           │                           │  + acknowledges    │
   │                           │                           │                    │
   │                           │ 3d. CopyStream RPC ends   │                    │
   │                           │     copy_op_tx dropped    │  copy_stream       │
   │◄── build_replica returns ─│     by SecondaryReceiver  │  returns None      │
   │                           │                           │  (stream ended)    │
   │                           │                           │                    │
   │                           │ ** COPY COMPLETE **       │                    │
   │                           │                           │                    │
   │ 4. change_role(Active)    │                           │                    │
   │──────────────────────────────────────────────────────►│                    │
   │                           │                           │──ChangeRole       │
   │                           │                           │  (Active)────────►│
   │                           │                           │  User starts      │
   │                           │                           │  draining          │
   │                           │                           │  replication_      │
   │                           │                           │  stream            │
   │                           │                           │                    │
   │ 5. update_catch_up_config │                           │                    │
   │──(on primary)────────────►│                           │                    │
   │                           │ PrimarySender::           │                    │
   │                           │   add_secondary(addr)     │                    │
   │                           │                           │                    │
   │                           │ 5a. Opens ReplicationStream│                   │
   │                           │     bidirectional gRPC ──►│                    │
   │                           │                           │                    │
   │                           │ 5b. Replays buffered ops  │                    │
   │                           │     (from build window)──►│──items flow into──►│
   │                           │                           │  repl_op_tx →      │
   │                           │                           │  replication_      │
   │                           │                           │  stream → user     │
   │                           │                           │  applies + acks    │
   │                           │                           │                    │
   │                           │ 5c. Live replication      │                    │
   │                           │     begins. Every new     │                    │
   │                           │     replicate() sends to  │                    │
   │                           │     this secondary too ──►│                    │
   │                           │                           │◄── ACK stream ────│
   │                           │                           │                    │
   │ 6. wait_for_catch_up      │                           │                    │
   │──(on primary)────────────►│                           │                    │
   │                           │ QuorumTracker waits       │                    │
   │                           │ until this secondary      │                    │
   │◄── catch-up done ────────│ has ACKed all pending ops │                    │
   │                           │                           │                    │
   │ 7. update_current_config  │                           │                    │
   │    (finalize quorum)      │                           │                    │
   │                           │                           │                    │
   │ ** SECONDARY IS NOW FULLY ACTIVE **                   │                    │
```

### Key Transitions

| Step | What Changes | Who Drives It |
|------|-------------|---------------|
| Build time | Both streams created (empty channels) | `PodRuntimeBuilder::build()` |
| Open | User receives both stream handles | `PodRuntime::handle_open()` |
| ChangeRole(Idle) | User starts draining `copy_stream` | Operator → PodRuntime → user |
| CopyStream RPC | Data flows into `copy_op_tx` → user's `copy_stream` | Primary's `handle_build_replica()` |
| CopyStream ends | `copy_op_tx` dropped → `copy_stream.get_operation()` returns `None` | Primary finishes copy |
| ChangeRole(Active) | User starts draining `replication_stream` | Operator → PodRuntime → user |
| add_secondary | `PrimarySender` opens ReplicationStream gRPC, replays buffer | Replicator actor |
| Live replication | Ops flow into `repl_op_tx` → user's `replication_stream` | Primary's `send_to_all()` |

### The Replication Queue (Bridging Copy and Replication)

During the copy window, the primary continues accepting writes. These
must reach the new secondary after the copy completes. The actor's
`ReplicationQueue` handles this (matching SF's `ReplicationQueueManager`):

```
Data path (every replicate):
  replication_queue.push(lsn, data)  → retains op in BTreeMap
  sender.send_to_all(lsn, data)     → non-blocking (unbounded channels)

UpdateCatchUpConfiguration (when new secondary connects, step 5):
  sender.add_secondary(replica_id, addr)  → opens gRPC stream
  pending = replication_queue.ops_from(1) → all retained ops
  for (lsn, data) in pending:
    sender.send_to_one(replica_id, lsn, data)  → replay to new secondary
  → then live ops flow via send_to_all

UpdateCurrentConfiguration (config finalized, step 7):
  replication_queue.gc(committed_lsn)  → remove committed ops
```

**Coverage with zero gap:**
- Copy delivers app state at snapshot time
- Queue replay delivers all ops retained since primary started
  (some overlap with copy — idempotent, same key = overwrite)
- Live replication delivers new ops after connection
- **No ops lost** — the queue retains everything until GC'd after
  the config is finalized.

**Non-blocking send_to_all:** Each secondary has a two-stage channel:
unbounded sender (never blocks actor) → background drain task →
bounded gRPC stream (may block, only blocks that secondary's task).
This matches SF's async job queue dispatch model.

### No Reconnection (Intentional)

Neither stream reconnects on failure:

- **Copy stream breaks** → `handle_build_replica` returns `Err` →
  operator retries entire `add_replica` from scratch. Secondary was
  `IdleSecondary`, state is discarded.
- **Replication stream breaks** → ACK reader exits silently, dead
  connection stays in `PrimarySender::connections`. See design gap B0
  for the detection/cleanup design work needed.

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

## How Catch-Up Works

Catch-up is the mechanism that ensures a new or restarted secondary has
received all committed operations before the configuration is finalized.
It bridges the gap between "secondary is connected and replicating" and
"secondary is fully caught up and safe to include in the quorum".

### The Three-Step Configuration Dance

Every configuration change (add replica, remove replica, failover,
switchover) goes through three steps on the primary's replicator:

```
Step 1: update_catch_up_configuration(new_config, old_config)
  │
  │  Enters dual-config mode:
  │  - CC (Current Config) = new_config
  │  - PC (Previous Config) = old_config
  │  - Writes must be ACKed by quorum from BOTH CC and PC
  │  - Records catch_up_baseline_lsn = highest_lsn at this moment
  │  - Seeds replica_acked_lsn from operator-reported progress
  │  - Marks must_catch_up replicas
  │
  ▼
Step 2: wait_for_catch_up_quorum(Write)
  │
  │  Blocks until:
  │  - All pending operations are committed (quorum met), AND
  │  - Every must_catch_up replica has individually ACKed
  │    all ops up to highest_lsn
  │  (If no new ops since step 1, returns immediately)
  │
  ▼
Step 3: update_current_configuration(new_config)
  │
  │  Exits dual-config mode:
  │  - CC = new_config (now the only config)
  │  - PC = empty
  │  - Clears must_catch_up_ids
  │  - Future writes only need quorum from CC
  │
  Done — configuration change is finalized
```

This is implemented in `PartitionDriver::reconfigure_quorum()` and
called by `add_replica`, `remove_secondary`, `failover`, and `switchover`.

### QuorumTracker Internals

The `QuorumTracker` (in `replicator/quorum.rs`) is the core data
structure. It tracks:

```rust
struct QuorumTracker {
    // Per-operation tracking
    pending: HashMap<Lsn, PendingOp>,    // ops waiting for quorum ACK
    committed_lsn: Lsn,                   // highest committed LSN
    highest_lsn: Lsn,                     // highest registered LSN

    // Configuration
    current_members: HashSet<ReplicaId>,   // CC member IDs
    current_write_quorum: u32,             // CC write quorum size
    previous_members: HashSet<ReplicaId>,  // PC member IDs (empty if not in reconfig)
    previous_write_quorum: u32,            // PC write quorum size

    // Catch-up tracking
    must_catch_up_ids: HashSet<ReplicaId>, // replicas that must individually catch up
    replica_acked_lsn: HashMap<ReplicaId, Lsn>,  // per-replica highest ACKed LSN
    catch_up_baseline_lsn: Lsn,           // highest_lsn at set_catch_up time
    catch_up_waiters: Vec<CatchUpWaiter>,  // blocked wait_for_catch_up callers
}
```

### Operation Lifecycle: register → ack → commit

```
Primary calls replicate(data):
  │
  ├─ Actor assigns LSN, calls QuorumTracker::register(lsn, primary_id, reply)
  │    ├─ Updates highest_lsn
  │    ├─ Creates PendingOp with acked_by = {primary_id}
  │    ├─ Checks: primary alone satisfies quorum? (single-replica case)
  │    │    YES → commit immediately, reply with Ok(lsn)
  │    │    NO  → insert into pending map, reply waits
  │    └─ Primary's own ACK tracked in replica_acked_lsn[primary_id]
  │
  ├─ PrimarySender::send_to_all() streams the op to all secondaries
  │
  ├─ Each secondary ACKs → ACK reader calls QuorumTracker::ack(lsn, replica_id)
  │    ├─ Updates replica_acked_lsn[replica_id] (max of old and new)
  │    ├─ Adds replica_id to PendingOp.acked_by
  │    ├─ Checks: is_quorum_met(acked_by)?
  │    │    Counts acked_by ∩ CC members ≥ CC write_quorum
  │    │    AND (if PC non-empty): acked_by ∩ PC members ≥ PC write_quorum
  │    │    YES → commit: update committed_lsn, send reply, notify catch-up waiters
  │    │    NO  → keep waiting
  │    └─ After commit: try_commit_pending() checks if other ops can now commit
  │
  └─ User's replicate() call resolves with Ok(lsn) or Err
```

### Catch-Up Completion Check

`wait_for_catch_up()` registers a waiter. On every commit and every ACK,
`notify_catch_up_waiters()` re-checks all waiters via `is_caught_up()`:

```rust
fn is_caught_up(mode) -> bool {
    // 1. All pending ops must be committed first
    if !pending.is_empty() { return false; }

    // 2. Baseline check: if no new ops since config change, trivially done
    if highest_lsn <= catch_up_baseline_lsn { return true; }

    // 3. Mode-specific per-replica check
    match mode {
        Write => {
            // Every must_catch_up replica must have ACKed up to highest_lsn
            for id in must_catch_up_ids {
                if replica_acked_lsn[id] < highest_lsn { return false; }
            }
        }
        All => {
            // Every member in CC must have ACKed up to highest_lsn
            for id in current_members {
                if replica_acked_lsn[id] < highest_lsn { return false; }
            }
        }
    }
    true
}
```

**Three conditions for catch-up (Write mode):**
1. `pending.is_empty()` — all ops committed (quorum met for each)
2. `highest_lsn <= catch_up_baseline_lsn` — trivial: no new ops since
   config change, OR:
3. For each `must_catch_up` replica: `replica_acked_lsn[id] >= highest_lsn`
   — the specific replica has individually ACKed everything

### Catch-Up Baseline (SF `previousConfigCatchupLsn`)

`catch_up_baseline_lsn` is set to `highest_lsn` at
`set_catch_up_configuration()` time. This is critical for correctness:

```
Example: Add replica 4 to a 3-replica set that has been running:

  Primary has committed LSNs 1-100 (highest_lsn = 100)
  set_catch_up_configuration called → catch_up_baseline_lsn = 100

  Replica 4 just finished copy (has all state up to LSN 100)
  Replica 4 connects to replication stream
  replica_acked_lsn[4] seeded from operator: 100

  New ops arrive: LSN 101, 102
  Replica 4 ACKs LSN 101 → replica_acked_lsn[4] = 101

  is_caught_up check:
    pending.is_empty()? → depends on LSN 102's quorum
    highest_lsn (102) > catch_up_baseline_lsn (100) → non-trivial
    must_catch_up replica 4: acked 101 < highest 102 → NOT caught up

  Replica 4 ACKs LSN 102 → replica_acked_lsn[4] = 102
    must_catch_up replica 4: acked 102 >= highest 102 → CAUGHT UP ✓
```

Without the baseline, a newly-added replica would need to ACK all 100
historical ops — ops it already received via the copy stream. The
baseline says "only check ops after this point".

### Progress Seeding from Operator

`set_catch_up_configuration` accepts `member_progress: HashMap<ReplicaId, Lsn>`
— the operator's knowledge of each replica's current progress (from
`ReplicaInfo::current_progress`). This seeds `replica_acked_lsn` for
new replicas that just finished copy, so the tracker knows they already
have data up to their copy LSN.

```rust
// In set_catch_up_configuration:
for (id, progress) in &member_progress {
    self.replica_acked_lsn
        .entry(*id)
        .and_modify(|v| { if *progress > *v { *v = *progress; } })
        .or_insert(*progress);
}
```

### Write vs All Mode

| Mode | What Must Be True | Used By |
|------|-------------------|---------|
| `Write` | Pending empty + each `must_catch_up` replica ACKed `>= highest_lsn` | `PartitionDriver` (all operations) |
| `All` | Pending empty + ALL `current_members` ACKed `>= highest_lsn` | SF legacy fallback (not used by driver) |

`Write` mode is the default. `All` mode is stricter (every member, not
just `must_catch_up` ones) and exists only for backward compatibility
with SF replicators that don't support `must_catch_up` markers.

### Notify Chain

Catch-up waiters are notified whenever state changes:

```
register() → if immediately committed → notify_catch_up_waiters()
ack()      → if quorum now met       → commit → notify_catch_up_waiters()
ack()      → even if op already committed → notify_catch_up_waiters()
             (late ACK from must_catch_up replica)
```

The last case is important: a `must_catch_up` replica might ACK an
already-committed op. The op was committed by quorum (primary + other
secondary), but `wait_for_catch_up` won't resolve until this specific
replica ACKs too. The late-ACK notification path handles this.
