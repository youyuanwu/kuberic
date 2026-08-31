# Kuberic: Replication Protocols

Protocols implemented by the durable operator workflows and
`PartitionDriver` (direct/non-operator orchestration), with
`WalReplicatorActor` as the pod-side replication engine.

> Part of the [Kuberic Design](../kuberic-replicator-design.md).

---

## Protocol: Failover

Unplanned primary failure. Direct/non-operator callers may still use
`PartitionDriver::failover()`. The production reconciler uses a versioned
`Failover` checkpoint in CRD status.

```
persist failover checkpoint with exact starting membership
record live election configuration on reachable replicas
collect incarnation/epoch/role/progress/deactivation evidence
assess previous and current read quorum independently
  → unavailable possible-best: WaitingForBestCandidate
  → recoverable missing quorum: QuorumLoss
  → quorum available: confirm deterministic target
  → quorum conclusively lost: confirm target + data-loss requirement
persist configuration epoch intent
persist data-loss epoch intent when required
apply and observe the combined epoch at the candidate
invoke/observe epoch-fenced durable OnDataLoss when required
  → NoStateChange: retain compatible survivors
  → StateChanged: refresh progress and narrow to primary-only
promote candidate → commit primary-only snapshot
distribute epoch → catch-up config → wait quorum → current config
record final election configuration and converge serving labels
attest exact target state
atomically publish stable snapshot + Healthy and clear operation
```

Every configuration denominator retains all persisted members; only validated
live observations enter the numerator. Stale deactivation evidence can count
toward read quorum but is filtered from primary candidacy. Candidate comparison
uses deactivation freshness, catch-up capability, LSN, retained range, and a
stable identity tie break.

Transiently unusable replies (incarnation/epoch mismatch, unhealthy, or
unknown retained progress) remain outstanding rather than becoming evidence
of permanent loss. Terminal role/range/configuration contradictions remain
excluded. A confirmed target may not fall below `minReplicas`.

`failoverDelay` is only a continuous primary-failure detection delay. It does
not end either Phase-1 wait or authorize data loss. Unavailable members are
probed in durable rotation. Data loss begins only when an applicable read
quorum is conclusively unavailable and no missing member can restore quorum or
outrank the survivor.

The candidate observes the advanced epoch before `OnDataLoss`; epoch equality
is checked again inside the replicator actor immediately before the provider
callback. Callback completion and result are status-observable after a lost
reply. Promotion is the irreversible boundary: later failures roll forward or
fail closed and never restore the failed primary.

---

## Protocol: Switchover

Planned primary change has two callers. Direct tests may still invoke the
monolithic `PartitionDriver::switchover()`. Reconciler-driven switchover uses a
versioned compact checkpoint in `status.operation`.

```
persist revoke intent → revoke writes
observe frozen LSN → wait for target catch-up
persist demote intent → demote old primary
persist promote intent → promote target
converge retained member epochs
install catch-up config → wait write quorum → install current config
converge routing labels
publish stable snapshot
```

Every external activity has a deterministic action ID and is persisted before
dispatch. A resumed reconcile observes first: a matching postcondition
advances, a matching precondition permits dispatch/retry, and any impossible
observation fails closed. The controller performs one status transition or one
activity dispatch per reconcile and uses Kubernetes `resourceVersion` to
exclude stale advancement.

If target promotion cannot be confirmed after old-primary demotion, the same
checkpoint durably restores the old primary at the new epoch, converges member
epochs/configuration and labels, then publishes the compensated stable
snapshot. Unverifiable post-promotion convergence becomes `poisoned`; it never
publishes a snapshot containing an old-epoch retained member.

The internal runtime activity endpoint records only the most recent completed
action ID/signature. This correlates a lost reply without introducing replay
history or exactly-once claims.

---

## Protocol: Create Partition

Reconciler-driven creation uses a durable `CreatePartition` checkpoint.
Direct tests and non-operator consumers may still invoke
`PartitionDriver::create_partition()`.

```
1. Validate pod-index IDs and pod UIDs; sort by logical ID
2. Persist explicit no-previous-topology intent
3. Open(New) and promote the lowest-ID replica
4. Commit primary-only current configuration (write quorum 1)
5. Persist the primary-only committed bootstrap snapshot
6. For each remaining replica in ID order:
   a. Open(New), update epoch, assign IdleSecondary
   b. build_replica (primary → idle, copy stream)
   c. change_role(ActiveSecondary)
   d. install catch-up configuration and wait for write quorum
   e. install current configuration
   f. persist the expanded committed bootstrap snapshot
7. Publish primary/secondary routing labels one member at a time
8. Publish the complete stable snapshot and transition Healthy
```

New pods use the non-serving `bootstrap` role label. A write-ahead fencing
phase also converts pre-existing initial serving labels before Open(New), so
the primary and secondary Services cannot route traffic during unsafe
bootstrap. The durable operation stores an optional committed bootstrap
snapshot; controller restart continues with the first uncommitted member and
never reopens or rebuilds a committed member.

Before primary-only configuration commits, failure cleanup may close/delete
the candidate and restart from no topology. After any bootstrap topology
commits, recovery rolls forward: configuration failure restores the committed
current configuration, then cleanup removes only the current candidate. A
changed committed-member incarnation poisons creation and defers to later
recovery semantics.

Routing fencing is label-only and never uses candidate runtime compensation.
The pending fence action targets the exact indexed member. A same-incarnation
pod that remains unavailable, or whose label cannot be fenced by deadline,
fails closed. A missing or replacement uncommitted member restarts creation
with a new target UID while preserving any committed prefix. Compensation is
rejected if its target appears in the committed bootstrap snapshot.

The direct driver workflow retains its existing SF-style ordering and grows
configuration one secondary at a time.

---

## Protocol: Scale-Up (Add Replica)

Add a new replica to a running partition. Direct/in-process callers may use
`PartitionDriver::add_replica()`. Operator reconciliation uses the durable
`AddingReplica` operation.

```
1. Operator creates new Pod + waits for Ready
2. Operator creates GrpcReplicaHandle for the new pod
3. Persist previous/target snapshots, exact candidate identity, phase, and the
   first pending action
4. Execute one write-ahead correlated activity per reconcile:
   a. Open(New)
   b. update_epoch
   c. change_role(IdleSecondary)
   d. build_replica (primary → idle, copy stream)
   e. change_role(ActiveSecondary)
   f. update_catch_up_configuration with `must_catch_up`
   g. wait_for_catch_up_quorum
   h. update_current_configuration
5. Label the candidate secondary and publish the target stable snapshot
```

The pod runtime records scheduled/in-progress/completed/failed state for the
current correlated activity. `BuildReplica` runs in the background so status
remains observable during a long copy, and exact duplicate delivery never
starts a second copy.

---

## Protocol: Remove Secondary

Healthy scale-down and permanent stale/dead-secondary eviction use one durable
`RemovingReplica` operation with `ScaleDown` or `Force` mode.

```
1. Validate a stable non-primary target, minReplicas, and retained old/target
   quorum
2. Persist previous and target snapshots plus exact runtime and pod UID
3. update_catch_up_configuration(target, previous) on the primary
4. wait_for_catch_up_quorum using the retained set
5. update_current_configuration(target)
6. Persist the target stable snapshot immediately after observing commit
7. RemoveReplica(target ID, exact old incarnation) on the primary
8. If the exact target is reachable: change_role(None), then Close
9. Delete the pod with its exact Kubernetes UID precondition
10. Complete cleanup and return to Healthy
```

Failures before step 5 restore the previous current configuration and previous
stable snapshot. Once target current configuration is observed, reconciliation
never republishes removed membership. Exact primary sender connections are
observable in `GetStatus`, so lost removal responses resume from connection
absence. Target lifecycle cleanup is conditional after membership commit; an
unreachable target does not block removal.

A ready replacement incarnation that is still required by desired capacity
uses durable rebuild instead. An unreachable old incarnation with no ready
replacement is force-removed; later scale-up can restore desired capacity.

---

## Protocol: Restart Secondary

Replace a failed secondary. Implemented in `PartitionDriver::restart_secondary()`.

```
1. Identify the old replica incarnation (Kubernetes Pod UID in production)
2. Close the old secondary handle
3. Remove the primary-side replication connection only when both the stable
   replica ID and old incarnation match
4. Operator deletes old Pod + creates new Pod
5. Wait for Ready
6. Add the new incarnation under the same stable replica ID
7. add_replica (full build from primary)
```

Replica-set configuration and status carry both `ReplicaId` and
`ReplicaInstanceId`. A new incarnation replaces any remaining primary-side
connection for the same stable ID and cancels the old drain/ACK tasks. A
delayed removal for the old incarnation is an idempotent no-op and cannot
remove the replacement.

The reconciler starts a durable rebuild after it sees a ready pod whose
incarnation differs from the stable secondary. It first retires the old exact
primary-side connection, then runs the durable add sequence under the same
logical replica ID. Before catch-up configuration, compensation removes and
closes the candidate. After catch-up begins it first restores the previous
current configuration. Once target current configuration is observed,
reconciliation rolls forward to target snapshot publication.

---

## Protocol: Stable Operator Restart Recovery

`status.stableSnapshot` is the sole authoritative recovery record. It contains
the committed epoch, primary logical ID, every stable member's logical ID,
pod-UID incarnation and typed role, and the majority write quorum. Addresses
and clients are never persisted.

On a `Healthy` reconcile with no process-local driver, the operator validates
pod-index labels and UIDs, constructs current handles, and invokes
`PartitionDriver::recover()`. Recovery calls `GetStatus` on each handle and
requires exact incarnation, epoch, role, primary, membership, and quorum
agreement before constructing driver state. Health is deliberately not a
snapshot invariant; normal health/failover handling runs after reconstruction.

No lifecycle, role, epoch, configuration, catch-up, build, removal, data-loss,
or write-status RPC is legal during recovery. Missing legacy snapshots and
all persisted/live mismatches fail closed. Recovery covers stable topology
only; it does not infer state for an in-progress reconfiguration or reopen a
restarted pod runtime.

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

run_build_replica_copy (after collecting state snapshot):
  copy_lsn = max LSN of snapshot items  → state provider's last_applied_lsn
  state.set_copy_lsn(replica_id, copy_lsn)  → stored in PartitionState

UpdateCatchUpConfiguration (when new secondary connects, step 5):
  sender.add_secondary(replica_id, addr)  → opens gRPC stream
  copy_lsn = state.take_copy_lsn(replica_id)  → retrieve boundary
  pending = replication_queue.ops_from(copy_lsn + 1)  → only post-copy ops
  for (lsn, data) in pending:
    sender.send_to_one(replica_id, lsn, data)  → replay to new secondary
  → then live ops flow via send_to_all

UpdateCurrentConfiguration (config finalized, step 7):
  replication_queue.gc(committed_lsn)  → remove committed ops
```

**Three ranges with zero gap (matching SF):**
```
[0, copy_lsn]              → Copy stream (from state provider snapshot)
(copy_lsn, highest_lsn]    → Replay from replication queue
(highest_lsn, ∞)           → Live replication (new ops via send_to_all)
```

**No ops lost, no duplicates** — the copy delivers state through `copy_lsn`,
the queue replays only ops beyond that boundary, and live replication
continues from the highest LSN. This is correct for both idempotent and
non-idempotent operations.

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
| Activate done | Granted | ReconfigPending |
| Configuration installed | Granted | **Granted** |

**During switchover (P→S on old primary):**

| Phase | Read | Write | Notes |
|---|---|---|---|
| Pre-catchup | Granted | **Granted** | SF catchup #1 (target catches up while writes flow) |
| Write revoked | Granted | ReconfigPending | `revoke_write_status()` — no new writes |
| Post-revoke catchup | Granted | ReconfigPending | SF catchup #2 (final drain) — **kuberic: E2 not yet implemented** |
| Role changed | NotPrimary | NotPrimary | `change_role(ActiveSecondary)` → `close_all()` |

**Runtime owns status transitions** — the replicator only writes LSN values
to `PartitionState`. Role transitions set the initial access state, and a
successful catch-up/current configuration update grants primary writes. See
`pod.rs::set_status_for_role()` and the configuration command handlers.

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
    quorum_timeout: Duration,              // per-operation/waiter deadline
    catch_up_failed: bool,                 // active catch-up attempt saw expiry
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
  ├─ Actor-owned scheduler expires the operation if its deadline arrives
  │    ├─ Removes it from pending
  │    ├─ Replies with Err(NoWriteQuorum)
  │    └─ Marks the active catch-up attempt failed
  │
  └─ User's replicate() call resolves with Ok(lsn) or Err(NoWriteQuorum)
```

The default quorum deadline is 5 seconds and can be overridden through
`WalReplicatorOptions`. ACK and expiration processing share the tracker
mutex. An ACK processed at or after its deadline cannot reverse a timeout.
`NoWriteQuorum` means quorum was not observed by the deadline; fewer than
quorum replicas may already have persisted the operation.

### Catch-Up Completion Check

Catch-up waiters also carry deadlines. They return `NoWriteQuorum` when their
deadline expires or when an operation expires during the active catch-up
attempt. Repeating `set_catch_up_configuration()` during the same attempt does
not move the baseline or clear failure. `set_current_configuration()` ends the
attempt; a later catch-up update then establishes a new baseline.

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
