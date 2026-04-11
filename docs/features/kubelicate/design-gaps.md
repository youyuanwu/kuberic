# Kubelicate: Design Gaps and Required Work

Gaps identified by reviewing the design docs against the implementation.
Categorized by severity and whether additional design work is needed
vs simple implementation of existing designs.

> Related: [Status & Roadmap](kubelicate/status.md),
> [Failure Scenarios](operator-failure-scenarios.md)

---

## Category A: Protocol Safety — Needs Design Work

These gaps affect data safety and correctness. The design docs don't
cover the error paths adequately. Additional design is required before
implementation.

### A1. Partial Failure in Multi-Replica Operations — ✅ Fixed

**Severity:** ✅ Resolved
**Affects:** `failover()`, `switchover()`
**File:** `kubelicate-core/src/driver.rs`

**Problem (was):** The switchover/failover had a "fence-before-promote"
pattern — `update_epoch` was called on ALL secondaries with `?`
(fail-on-first-error) BEFORE promotion. An unreachable secondary blocked
the entire operation and left the partition in an inconsistent state
(write status revoked but never rolled back).

**Root cause:** The fence-before-promote pattern was incorrect — SF doesn't
do it. In SF, epoch distribution happens in Phase 4 (ACTIVATE), AFTER
the new primary is active. Pre-promotion fencing is unnecessary because:
- **Failover:** old primary is dead → can't send ops
- **Switchover:** old primary's writes are revoked → can't send new ops

**Fix:** Restructured both `failover()` and `switchover()` to match SF's
Phase 4 pattern:
1. Promotion happens first (with new epoch delivered to promoted replicas)
2. Epoch distributed to other secondaries **after** promotion, best-effort
3. Unreachable secondaries are skipped with a warning (rebuilt later)

**Switchover flow (now):**
1. `revoke_write_status()` on old primary (A2 fix)
2. `change_role(ActiveSecondary)` on old primary
3. `change_role(Primary)` on target (Phase 4)
4. Best-effort `update_epoch` on other secondaries
5. Reconfigure quorum + catchup

**Failover flow (now):**
1. Select best candidate by LSN
2. `change_role(Primary)` on winner (Phase 4)
3. Best-effort `update_epoch` on other secondaries
4. Reconfigure quorum + catchup

**Test:** Switchover with one secondary closed succeeds — unreachable pod
is skipped during epoch distribution, new primary is active and serving.

---

### A2. Switchover Write Revocation — ✅ Fixed

**Severity:** ✅ Resolved
**Affects:** `switchover()`
**File:** `kubelicate-core/src/driver.rs`

**Problem (was):** The switchover jumped directly to `change_role(ActiveSecondary)`
without revoking writes first. During the gap between epoch fencing and
demotion, the old primary could still accept and commit writes.

**Fix:** Added `RevokeWriteStatus` RPC to the control plane. The switchover
now calls `revoke_write_status()` on the old primary as its **first step**,
before epoch fencing and demotion. This sets `write_status = ReconfigurationPending`,
immediately rejecting new writes via the existing fast-path check in
`replicate()`. In-flight writes continue to completion and are failed by
demotion's `fail_all()`.

**Switchover sequence (now):**
1. `revoke_write_status()` on old primary → new writes rejected immediately
2. Fence secondaries with new epoch
3. Demote old primary → `change_role(ActiveSecondary)` → in-flight writes failed
4. Promote target → `change_role(Primary)`
5. Rebuild configuration + catch-up

**Changes made:**
1. Proto: `RevokeWriteStatus` RPC + request/response messages
2. `ReplicaHandle` trait: `revoke_write_status()` method
3. `GrpcReplicaHandle`: calls RPC
4. `InProcessReplicaHandle`: sets `PartitionState` directly
5. `PodRuntime`: `RuntimeCommand::RevokeWriteStatus` + handler
6. `ControlServer`: gRPC handler
7. `switchover()`: calls `revoke_write_status()` as step 1

**Remaining consideration:** SF's SwapPrimary has a double-catchup pattern
(catchup WITH writes → revoke → catchup WITHOUT writes → promote). Our
implementation skips the first catchup-with-writes phase. For most workloads
this is fine — the write revocation + catch-up at step 5 is sufficient.
Double-catchup would reduce the catch-up window after revocation for
high-throughput workloads.

**Test:** Concurrent writer during `driver.switchover()` checks for
`ReconfigurationPending` error messages. Verified: test fails without fix
(`reconfig=0`), passes with fix (`reconfig>0`).

---

### A3. Promotion Failure Leaves Partition Unavailable — ✅ Fixed (switchover)

**Severity:** ✅ Switchover fixed / 🟡 Failover handled by reconciler retry
**Affects:** `switchover()`
**File:** `kubelicate-core/src/driver.rs`

**Problem:** If `change_role(Primary)` on the target fails during
switchover, the old primary is already demoted. No replica can accept
writes. The driver returns `Err` but doesn't rollback the demotion.

For failover, this is less severe: the driver returns `Err` and the
reconciler retries (matching SF's FM retry-via-new-reconfiguration
pattern). The reconciler re-evaluates which replicas are alive and
picks a different candidate.

**SF reference:** SF has explicit `AbortPhase0Demote` and
`RevertConfiguration()` in the RA. When a swap is aborted (target dies
or higher-priority reconfig arrives), SF:
1. Cancels the catchup (`SendCancelCatchupMessage`)
2. Enters `AbortPhase0Demote` state
3. Calls `RevertConfiguration()` — restores all replicas' roles to their
   pre-swap state (CC role = PC role)
4. Reports `ReconfigurationResult::AbortSwapPrimary`

SF doesn't retry inside the reconfiguration — the FM's outer loop handles
retry by triggering a new reconfiguration.

**Fix (switchover):** Target promotion is wrapped in a 5s timeout. On
failure or timeout, the driver re-promotes the old primary via
`change_role(new_epoch, Primary)` — matching SF's `RevertConfiguration()`
pattern. The old primary becomes primary in the new epoch, which is safe.

**Remaining:** After rollback, the quorum configuration is stale (still
expects secondary ACKs from the old config). The reconciler must
reconfigure the quorum on the next reconcile cycle. The partition is
recoverable (primary exists) but writes hang until reconfigured.

**Test:** Close the switchover target before switchover → promotion fails
→ verify old primary is re-promoted (primary_id restored).

**Design:**
- **Switchover:** If target promotion fails, rollback:
  ```
  if change_role(Primary, target) fails:
      change_role(Primary, old_primary)  // re-promote
      // old primary still has the new epoch — that's fine,
      // it becomes primary in the new epoch
  ```
- **Failover:** Return `Err`, let reconciler retry with a different
  candidate. This already works with the current code + reconciler.

**Operator crash recovery (related):**

If the operator crashes mid-switchover, no rollback happens. The
`PartitionDriver` is in-memory only. On restart, the reconciler must
reconstruct state from pod `GetStatus` queries:
- Pod with `role=Primary` + highest epoch → that's the primary
- No pod with `role=Primary` → treat as primary failure → failover
- Pods with stale epochs → need rebuild

This "no primary → failover" pattern naturally handles mid-switchover
crashes: the old primary was demoted, no new primary promoted, so the
reconciler triggers failover and promotes the best available replica.

The `PartitionDriver::recover()` method (D item, not yet implemented)
would formalize this query-and-reconstruct pattern.

---

### A4. gRPC Control Plane Ordering — ✅ Not a Problem

**Severity:** ✅ Closed (no issue)
**Affects:** N/A

**Original concern:** gRPC/HTTP2 multiplexing could reorder control RPCs.

**Analysis:** The driver calls all RPCs **sequentially** — each `.await`
completes before the next RPC is sent. There are never concurrent RPCs
to the same handle. HTTP2 multiplexing can only reorder concurrent
requests; sequential request-response pairs are inherently ordered.

The only exception is `abort()` (fire-and-forget via `tokio::spawn`),
which is a last-resort shutdown where ordering doesn't matter.

**Conclusion:** No fix needed. A mutex would add complexity for zero
benefit since the driver is single-threaded per-partition.

---

### A5. Synchronous BuildReplica and CatchUp Block Operator

**Severity:** 🔴 Critical (for large datasets) / 🟢 OK for MVP
**Affects:** `PartitionDriver::add_replica()`, `reconfigure_quorum()`,
`GrpcReplicaHandle::build_replica()`, `wait_for_catch_up_quorum()`
**Files:** `driver.rs`, `handle.rs`, `pod.rs`, `server.rs`, `quorum.rs`

**Problem:** Two long-running operations block the operator synchronously:

1. **`build_replica`** — the operator calls a single gRPC unary RPC that
   holds open for the entire copy duration. For GBs of state, this is
   minutes to hours.

2. **`wait_for_catch_up_quorum`** — blocks until the must_catch_up replica
   ACKs all ops. If the replica is slow (large backlog) or dead (stream
   broke — see B0), this hangs forever.

Both have the same consequences:
- **Reconciler stall** — blocked on `.await`, no health checks or
  failover detection can proceed.
- **Operator restart** — drops the gRPC connection, aborts mid-operation,
  no way to resume.

**SF's approach (from C++ source):**

SF uses a **fire-and-retry** pattern, NOT progress-based polling:

1. **RA → RAProxy:** RA sends `ReplicatorBuildIdleReplica` message (async)
2. **RAProxy → Replicator:** Calls `BeginBuildIdleReplica()` (COM async)
3. **In-progress handling:** If the build is still running, the RAProxy
   returns `RAProxyBuildIdleReplicaInProgress` and **does not reply** to
   the RA. The RA's `MessageRetryTimer` re-sends the message periodically.
   The RAProxy drops duplicates (`MessageDropInvalidWorkInProgress`).
4. **Completion:** When the build finishes, the RAProxy sends the reply.
5. **Cancellation:** RA can send `CancelCatchupReplicaSet` to abort.

Key insight: **no progress reporting** — just "done" or "still working."
The FM monitors overall health (via heartbeats), not build progress.

**Design (SF-aligned fire-and-retry):**

```
Current (synchronous, blocking):
  operator ──BuildReplica RPC──► primary (blocks for hours) ──► response
  operator ──WaitForCatchUp RPC──► primary (blocks forever) ──► response

SF-aligned (fire-and-retry):
  operator ──BuildReplica RPC──► primary (returns immediately)
  operator ──GetStatus RPC──► primary (replica state: InBuild/Ready)
  operator ──GetStatus RPC──► primary (replica state: Ready → done!)

  Reconciler loop (every 10s):
    1. Poll GetStatus on primary
    2. If replica is Ready → proceed to next step
    3. If replica is InBuild → requeue, keep waiting
    4. If primary unreachable → cancel build, restart
```

**Implementation approach (simpler than previous design):**
- `BuildReplica` RPC returns immediately after starting the background
  copy task (which already runs in a spawned task — C0 fix).
- `GetStatus` response adds per-replica build state (InBuild/Ready).
- `wait_for_catch_up_quorum` stays as-is for now (short for small data),
  but the reconciler wraps it with a timeout and requeue.
- No progress fields needed for MVP — just state transitions.
- Stall detection is the reconciler's health check timeout, not a
  per-operation progress tracker.

**This is a future architectural change.** Current synchronous approach
is correct for MVP and small datasets. All tests pass with it.

### A6. Uncommitted Operations Not Rolled Back on Epoch Change

**Severity:** 🔴 High (data corruption for non-idempotent operations)
**Affects:** `SecondaryReceiver`, user services, `StateProviderEvent`
**Files:** `secondary.rs`, `events.rs`, kv-stateful `service.rs`

**Problem:** When a secondary receives operations via the replication
stream, it dispatches them to the user service immediately. The user
applies the operation to its state and calls `acknowledge()`, which
triggers the ACK back to the primary. However, the operation may never
be committed (quorum not met before primary crashes).

On epoch change, the replicator truncates its in-memory log back to
`committed_lsn` (`SecondaryState::update_epoch()` at `secondary.rs:72-78`).
But the **user service's state is NOT rolled back** — it still contains
the uncommitted operation.

**Scenario:**
```
1. Primary has LSN 5 committed, sends LSN 6 to secondaries
2. Secondary applies LSN 6 to user state, calls acknowledge()
3. Primary crashes BEFORE quorum for LSN 6 is met
4. Failover: secondary promoted with new epoch
5. Replicator truncates log: removes LSN 6 from in-memory queue
6. User service still has LSN 6 applied ← INCONSISTENT
```

For idempotent operations (key-value puts), this is usually harmless.
For non-idempotent operations (counters, queues, financial ledgers),
this causes data corruption.

**SF solution:** `IStateProvider::UpdateEpoch(epoch, previousEpochLastLsn)`
callback tells the user service to truncate state beyond
`previousEpochLastLsn`. The user is responsible for rollback. For V2
(Reliable Collections), the `LoggingReplicator` handles this
automatically by undoing uncommitted transactions from its WAL.

**Our current code (KV example) — no rollback:**
```rust
StateProviderEvent::UpdateEpoch { previous_epoch_last_lsn, reply, .. } => {
    info!(previous_epoch_last_lsn, "epoch updated");
    let _ = reply.send(Ok(()));  // No rollback!
}
```

**Fix options (choose one):**

1. **User-side rollback (SF V1 model):** Document that user services
   MUST implement rollback in `UpdateEpoch`. Pass `previous_epoch_last_lsn`
   and require user to discard state beyond that LSN. Hard for arbitrary
   state stores (how do you "undo" a HashMap insert?).

2. **Defer user dispatch until committed (simplest):** Secondary ACKs
   to primary after WAL persist (Option C), but does NOT dispatch to
   user's `OperationStream` until the operation is committed. Committed
   notification comes when primary's `committed_lsn` advances. No
   rollback ever needed because user only sees committed ops. Adds
   latency (user apply waits for quorum round-trip).

3. **Framework WAL rollback (Option C complement):** The replicator
   WAL tracks committed vs uncommitted entries. On epoch change,
   truncate WAL beyond `committed_lsn`. On user service restart, replay
   only committed WAL entries. Uncommitted ops never reach user state
   because the WAL is the source of truth for replay.

**Recommendation:** Option 2 for safety (user never sees uncommitted
ops), with Option 3 as the WAL-based complement. Option 2 changes the
secondary ACK semantics:
- Current: ACK = "user applied + acknowledged"
- New: ACK = "WAL persisted" (fast), user apply happens after commit

This aligns with Option C in `wal-persistence.md` — the two-level ACK
model where WAL ACK gates quorum, and user apply happens after commit.

---

These gaps affect reliability under adverse conditions. The system may
hang, leak resources, or fail to recover.

### B0. Replication Stream Failure Goes Undetected

**Severity:** 🟡 Medium (reduced — `send_to_all` cleanup already implemented)
**Affects:** `PrimarySender`, `WalReplicatorActor`, operator reconciler
**Files:** `primary.rs`, `actor.rs`, `reconciler.rs`

**Problem:** The replication stream is a long-lived bidirectional gRPC
stream. When it breaks, detection is partial:

**What's already fixed:**
- `send_to_all()` (primary.rs:163-175) detects dead channels via
  `item_tx.send().is_err()`, logs a warning, and **removes the dead
  connection** from `PrimarySender::connections`. This was implemented
  during the non-blocking send work.

**No auto-reconnection (intentional, matching SF):**
When a connection is removed, the secondary silently drops out of
replication. We do NOT auto-reconnect because:
1. The pod might be truly dead — reconnection retries would be futile
2. Even if reconnected, the secondary missed ops — its data is stale
   and needs a full rebuild (copy), not just reconnection
3. Auto-reconnection would bypass the operator's lifecycle control

SF handles this the same way: the replicator stops sending, the FM
detects the replica is unreachable (via heartbeat failure), and
orchestrates a `BuildIdleReplica` rebuild. The replicator never
reconnects on its own.

**The missing piece:** We remove the dead connection but **don't notify
the operator**. The operator's reconciler checks pod readiness (K8s),
not replication stream health. A pod can be Running/Ready (process alive)
but its replication stream dead (e.g., network partition between pods).

**What's still a gap:**

1. **ACK reader exits silently** — the spawned ACK reader task
   (primary.rs:93) logs a warning and exits. No callback to the actor.
   However, the dead connection IS removed on the next `send_to_all()`
   call (send fails → cleanup). So the gap is timing: between the ACK
   reader dying and the next write, the connection appears alive.

2. **QuorumTracker has no per-operation timeout** — if ALL secondaries
   die, `replicate()` hangs forever. With 3 replicas (quorum=2), losing
   one secondary is fine (quorum still met). Losing both means writes
   hang. In practice, the reconciler detects pod failures (NotReady) and
   triggers failover before both streams die simultaneously.

3. **Operator can't detect replication health** — pods can be Ready but
   replication broken. `GetStatus` doesn't report replication stream
   status. The reconciler only checks pod readiness.

**Remaining design (for production hardening):**

- **Per-operation timeout in QuorumTracker:** `register()` should accept
  a timeout. If quorum ACK doesn't arrive, fail with `NoWriteQuorum`
  instead of hanging. This is the most important remaining fix.

- **Replication health in GetStatus:** Add `connected_secondaries` count
  or per-replica replication status to the GetStatus response. The
  reconciler can then detect "Ready but replication broken."
  This maps to SF's RA→FM `ChangeNotification` pattern — the pod
  reports its health, the operator acts on it.

- **ACK reader death notification** (nice-to-have): When the ACK reader
  exits, notify the actor to proactively remove the connection. Currently
  cleanup happens lazily on the next `send_to_all()`, which is fine for
  most workloads.

**SF failure detection model (for reference):**

SF does NOT monitor replication streams directly. It uses layered detection:

| Layer | Mechanism | Detects | Speed |
|-------|-----------|---------|-------|
| Federation | Lease-based heartbeats between nodes | Node failure | Seconds |
| RA → FM | `ReplicaDown` message when local process crashes | Process crash | Seconds |
| FM health | Periodic `ChangeNotification` from each RA | Silent node death | ~30s |

A broken replication stream without node/process failure (network partition
between pods) is detected indirectly: quorum loss → writes fail → user
reports fault. This is a known gap in SF too, though SF's Federation
leases catch most real failures at the node level.

**Our K8s equivalents:**
- Node-level: K8s kubelet → pods go NotReady
- Pod-level: Liveness probe → pod restart
- Replication-level: **Our gap** — no equivalent of Federation leases
  between pods. Fix: `connected_secondaries` in GetStatus (see above).

---

### B1. Copy Protocol — No Reconnection (Intentional)

**Severity:** 🟡 Medium (streaming improvement) / 🟢 Low (reconnection)
**Affects:** `handle_build_replica()`, `SecondaryReceiver`
**File:** `kubelicate-core/src/pod.rs`

**No reconnection is intentional.** If the `copy_stream` gRPC call fails
mid-transfer (network error, pod crash), the error propagates up:

```
copy_stream fails (pod.rs:579)
  → handle_build_replica returns Err
    → driver.add_replica returns Err (handle NOT added)
      → reconciler requeues after 5s
        → next reconcile retries add_replica from scratch
          → new channel, new GetCopyContext, full rebuild
```

This matches SF's model: `BuildReplica` is a single atomic operation.
If it fails for any reason, the operator retries the entire build. The
secondary's state is discarded (it was in `IdleSecondary` role, not yet
promoted to `ActiveSecondary`), so there's no partial-state corruption.

**Why no mid-stream reconnection:**
- The copy is orchestrated by the primary's `PodRuntime`, not the operator.
  The primary connects to the secondary as a gRPC client. If the stream
  breaks, the primary has no way to know what the secondary received.
- Reconnecting mid-copy would require the secondary to report its
  received-LSN checkpoint, which adds protocol complexity for a rare case.
- The retry path already handles it: the secondary's `GetCopyContext`
  callback can report its current LSN, and the primary's `GetCopyState`
  can produce only the delta. This is an application-level concern
  (the kv-stateful example currently always sends full state, but
  the protocol supports incremental).

**Remaining improvement — streaming instead of materializing:**
`collect_stream()` materializes the entire copy state into a
`Vec<proto::CopyItem>` before sending. For large datasets this causes
memory exhaustion. Should be replaced with a direct pipe:
`OperationStream` → `tokio_stream::Stream` → gRPC streaming request.

---

### B2. Timeout Enforcement for Short RPCs

**Severity:** 🟠 High
**Affects:** Short control-plane RPCs, state provider callbacks
**Files:** `handle.rs`, `pod.rs`, `actor.rs`

**Problem:** Short RPCs (`update_epoch`, `change_role`, `open`, `close`)
have no timeout. A crashed or unreachable pod causes these calls to hang
indefinitely, blocking the entire failover or switchover protocol.

**Note:** Long-running operations (`build_replica`, `wait_for_catch_up`)
are addressed by A5 (async with progress-based stall detection). This
gap covers only short RPCs that should complete in milliseconds.

**Design needed:**
- **Per-RPC timeout on `GrpcReplicaHandle`:** Add `timeout` field
  (default 10s), wrap every control RPC in `tokio::time::timeout()`:
  ```rust
  async fn update_epoch(&self, epoch: Epoch) -> Result<()> {
      tokio::time::timeout(self.timeout, self.client.clone().update_epoch(...))
          .await
          .map_err(|_| KubelicateError::Timeout)??;
      Ok(())
  }
  ```
- **State provider callback timeout:** `send_state_provider()` already
  has a 30s reply timeout (line 648-651 in pod.rs), but only for the
  oneshot reply — not for slow user processing. Add cancellation token
  support (see C3).
- **Configuration:** `spec.rpcTimeout` (default 10s) in CRD spec.

---

### B3. gRPC Handle Reconnection and IP Resolution

**Severity:** 🟠 High
**Affects:** `GrpcReplicaHandle`
**File:** `kubelicate-core/src/grpc/handle.rs`

**Problem:** Channel is created once with a hardcoded IP address. Pod
restart assigns a new IP. The handle becomes a zombie — all RPCs fail
but the handle isn't replaced.

**Design needed:**
- **DNS-based addressing:** Use K8s DNS names instead of pod IPs:
  `{pod-name}.{headless-service}.{namespace}.svc.cluster.local`.
  tonic's Channel resolves DNS on each connection attempt.
- **Lazy connection:** Use `Channel::from_shared(addr)` without
  `.connect().await` — tonic connects lazily on first RPC. This
  naturally handles reconnection after pod restarts.
- **Handle staleness detection:** The operator should detect when a
  handle's underlying pod has a different IP (pod recreated) and
  replace the handle. This is part of the "missing pod detection"
  design in operator-failure-scenarios.md §3.

---

### B4. Reconciler Operation Locking

**Severity:** 🟠 High
**Affects:** `reconciler.rs`
**File:** `kubelicate-operator/src/reconciler.rs`

**Problem:** If a reconciliation triggers `add_replica` (which takes
seconds due to copy), and the controller requeues another reconciliation
before it completes, the second reconciliation may try to add the same
replica again or modify driver state while the first is mid-operation.

**Design needed:**
- **CRD-status-based lock:** Add `status.operationInProgress: Option<String>`
  to the CRD. Set before starting long operations (failover, add_replica,
  switchover). Clear on completion. If set on entry, skip reconciliation
  (requeue after 5s).
  ```rust
  if set.status.operation_in_progress.is_some() {
      return Ok(ReconcileAction::Requeue(Duration::from_secs(5)));
  }
  ```
- **Generation counter:** Add `status.observedGeneration` to track which
  spec generation the operator has processed. Standard K8s pattern.
- **Idempotent operations:** Ensure that re-running a partially completed
  operation is safe. For `add_replica`: check if the replica is already
  open before calling `open()` again.

---

## Category C: Correctness Refinements — Needs Design Work

Smaller issues that affect correctness in edge cases.

### C0. Replication Queue Replay — ✅ Fixed

**Severity:** ✅ Resolved
**Affects:** `WalReplicatorActor`, `ReplicationQueue`, `PartitionState`, KV service

**What was fixed (Phase 1 — ReplicationQueue):**
The in-flight ops gap is closed — all ops are retained in the `ReplicationQueue`
and replayed to new replicas at `add_secondary` time. No data loss.

**What was fixed (Phase 2 — Precise LSN boundary):**
The replay now uses the precise copy boundary LSN instead of replaying
everything from LSN 1. Two changes:

1. **Copy LSN tracking (`PartitionState.copy_lsn_map`):** After
   `run_build_replica_copy` collects the state snapshot, it extracts the
   copy boundary LSN (max LSN of copy items = `last_applied_lsn` at
   snapshot time) and stores it per-replica via `state.set_copy_lsn()`.
   At `UpdateCatchUpConfiguration` time, the actor reads it via
   `state.take_copy_lsn()` and replays only `ops_from(copy_lsn + 1)`.

2. **Copy stream full consumption:** The KV service's role change handler
   was cancelling the copy drain task when transitioning from IdleSecondary
   to ActiveSecondary, losing items still in the channel. Fixed by waiting
   for the copy drain to finish naturally (sender is already dropped by
   the gRPC handler) before starting the replication drain. This ensures
   the secondary receives the complete copy state before replication begins.

**Three ranges with zero gap (matching SF):**
```
[0, copy_lsn]              → Copy stream (from state provider snapshot)
(copy_lsn, highest_lsn]    → Replay from replication queue
(highest_lsn, ∞)           → Live replication (new ops via send_to_all)
```

**Changes made:**
1. `PartitionState` (`handles.rs`): added `copy_lsn_map: Mutex<HashMap<ReplicaId, Lsn>>`
   with `set_copy_lsn()` and `take_copy_lsn()` methods
2. `run_build_replica_copy` (`pod.rs`): extracts `copy_lsn` from collected state items,
   stores via `state.set_copy_lsn(replica.id, copy_lsn)`
3. Actor (`actor.rs`): at `UpdateCatchUpConfiguration`, reads `state.take_copy_lsn(&member.id)`
   and replays from `copy_lsn + 1` instead of `1`
4. KV service (`service.rs`): on IdleSecondary → ActiveSecondary transition,
   waits for copy drain to complete naturally instead of cancelling

**Why this is correct for non-idempotent ops:**
- The copy snapshot captures all state through `last_applied_lsn` at snapshot time
- The actor's queue LSNs match the app's LSNs (assigned by the same actor)
- `ops_from(copy_lsn + 1)` replays only ops NOT in the snapshot
- No overlap, no gap

**Remaining consideration:** User apps that handle the IdleSecondary →
ActiveSecondary role change must ensure the copy stream is fully consumed
before starting replication. The KV app demonstrates the correct pattern.
This could be enforced at the framework level in the future (e.g., the
framework waits for the copy stream to drain before delivering the
replication stream).

**Verified:** 5/5 passes at 500 initial + 200 concurrent writes.

### C1. QuorumTracker Stale ACK Entries

**Severity:** 🟡 Medium
**Affects:** `QuorumTracker`
**File:** `kubelicate-core/src/replicator/quorum.rs`

**Problem:** `set_current_configuration` clears `previous_members` but
doesn't clean up `replica_acked_lsn` entries for replicas that are no
longer in any configuration. Late ACKs from removed replicas could
affect catch-up calculations.

**Fix:** In `set_current_configuration()`, retain only entries where
`replica_id ∈ current_members`:
```rust
self.replica_acked_lsn.retain(|id, _| self.current_members.contains(id));
```

---

### C2. Double set_catch_up_configuration Guard

**Severity:** 🟡 Medium
**Affects:** `QuorumTracker`
**File:** `kubelicate-core/src/replicator/quorum.rs`

**Problem:** Calling `set_catch_up_configuration` twice without an
intervening `set_current_configuration` overwrites `catch_up_baseline_lsn`.
The second call's baseline is higher, making catch-up fire too early.

**Fix:** Add a guard:
```rust
pub fn set_catch_up_configuration(...) -> Result<()> {
    if !self.previous_members.is_empty() {
        return Err(KubelicateError::InvalidState(
            "catch-up configuration already active".into()
        ));
    }
    // ... existing logic
}
```

---

### C3. Cancellation Tokens for State Provider Callbacks

**Severity:** 🟡 Medium
**Affects:** `StateProviderEvent`, `PodRuntime`
**Files:** `events.rs`, `pod.rs`

**Problem:** `GetCopyState` and `GetCopyContext` callbacks don't pass
a `CancellationToken`. If the user's state production hangs (large
dataset, deadlock), there's no way to abort the build operation.

**Fix:** Add `CancellationToken` to the relevant `StateProviderEvent`
variants:
```rust
GetCopyState {
    up_to_lsn: Lsn,
    copy_context: OperationStream,
    token: CancellationToken,  // NEW
    reply: oneshot::Sender<Result<OperationStream>>,
},
```

---

## Category D: Already Designed — Implementation Only

These items have complete designs in the existing docs. No additional
design work needed — just implementation.

| Item | Design Location | Priority |
|------|----------------|----------|
| Data loss protocol (`on_data_loss()` + `data_loss_number`) | operator-failure-scenarios.md §1, §7 | P0 |
| Operator restart recovery (`PartitionDriver::recover()`) | operator-failure-scenarios.md §8, A3 notes above | P0 |
| Secondary health detection + replacement | operator-failure-scenarios.md §2 | P0 |
| Missing pod detection | operator-failure-scenarios.md §3 | P0 |
| gRPC failure tracking (per-replica counter) | operator-failure-scenarios.md §5 | P1 |
| Failover delay (`failingSinceTimestamp`) | operator-failure-scenarios.md §1 | P1 |
| `force_remove_secondary` (error-tolerant removal) | operator-failure-scenarios.md §3 | P1 |
| CRD conditions (Ready, Degraded, QuorumAvailable) | operator-failure-scenarios.md | P1 |
| Old primary cleanup after failover | operator-failure-scenarios.md §9 | P1 |
| Primary self-fencing liveness probe | operator-failure-scenarios.md §5 | P2 |
| Node drain detection | operator-failure-scenarios.md §6 | P2 |
| Multi-primary detection | operator-failure-scenarios.md §9 | P2 |
| CrashLoop retry capping | operator-failure-scenarios.md §4 | P3 |
| Pod anti-affinity in CRD | operator-failure-scenarios.md §6 | P3 |

---

## Summary

| Category | Count | Top Priority |
|----------|-------|-------------|
| A: Protocol Safety | 6 | **A1 ✅**, **A2 ✅**, **A3 ✅ (switchover)**, **A4 ✅ (not an issue)**, A5 async build, **A6 uncommitted rollback** |
| B: Operational Resilience (needs design) | 5 | B0 partially fixed (QuorumTracker timeout remaining), B2 timeouts |
| C: Correctness Refinements (needs design) | 4 | **C0 ✅ fixed**, C1 stale ACKs |
| D: Already Designed (implement only) | 14 | Data loss protocol, operator restart |
| **Total** | **29** | |

**Recommended order:**
1. **A6 uncommitted rollback** — data corruption for non-idempotent ops
2. B0 QuorumTracker timeout — prevents write hangs on dual stream failure
3. **A5 (async build, SF fire-and-retry)** — blocks reconciler on large datasets
4. B2 (short RPC timeouts) — prevents hangs on control-plane calls
5. A3 failover candidate retry — failover resilience
6. D items P0 (data loss, operator restart, secondary health)
7. B3 + B4 (reconnection, operation locking) — operational maturity
8. B1 streaming improvement — large dataset support
9. C1-C3 (refinements) — can be done alongside other work
