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

### A3. Promotion Failure Leaves Partition Unavailable

**Severity:** 🔴 Critical
**Affects:** `failover()`, `switchover()`
**File:** `kubelicate-core/src/driver.rs`

**Problem:** After all secondaries are fenced with the new epoch, if
`change_role(Primary)` on the winner fails (network error, pod crash),
no replica can accept writes. Secondaries reject old-epoch ops, but
no primary exists in the new epoch. Partition is permanently
write-unavailable until operator intervention.

**Design needed:**
- **For failover:** Retry promotion with exponential backoff (the
  candidate might be temporarily unreachable). If candidate is truly
  dead, select next-best candidate and retry.
  ```
  candidates = sorted by LSN (descending)
  for candidate in candidates:
      if change_role(Primary) succeeds:
          break
      else:
          mark candidate as failed, try next
  if all candidates fail:
      enter degraded state, requeue
  ```
- **For switchover:** If target promotion fails, rollback:
  re-promote the old primary (it was only demoted, not fenced).
  ```
  if change_role(Primary, target) fails:
      change_role(Primary, old_primary)  // rollback
      update_epoch back to previous      // unfence
  ```
- The driver needs a `failover_with_retry()` method that implements
  the candidate loop, separate from the current `failover()`.

---

### A4. gRPC Control Plane Ordering

**Severity:** 🔴 Critical
**Affects:** All driver → replica communication
**File:** `kubelicate-core/src/grpc/handle.rs`

**Problem:** Control RPCs (`update_epoch`, `change_role`, `open`, etc.)
share a single tonic channel. gRPC/HTTP2 multiplexing can reorder
requests. If `change_role` arrives before `update_epoch`, the replica
processes the role change in the old epoch — violating the
fence-before-promote invariant.

**Design needed:**
- **Option 1: Per-handle mutex.** Wrap the control client in a
  `tokio::sync::Mutex`. Every control RPC acquires the lock first.
  Simple, correct, minor contention (control RPCs are rare).
- **Option 2: Sequence numbers.** Add a monotonic sequence number to
  every control RPC request. Server-side rejects out-of-order requests
  with a retry hint. More complex, handles network reordering.
- **Recommendation:** Option 1 (mutex) for now. The driver already
  calls RPCs sequentially (not concurrently), so contention is zero.
  The mutex is a safety net against future parallelization.

---

### A5. Synchronous BuildReplica and CatchUp Block Operator

**Severity:** 🔴 Critical (for large datasets)
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
- **Cannot distinguish slow vs stuck** — a fixed timeout doesn't work
  because a 10GB copy that takes 2 hours is legitimate.

**SF doesn't have this problem:** SF's FM sends commands asynchronously.
The RA reports progress, the FM monitors. The FM never blocks on a
single operation.

**Design needed — async operations with progress-based stall detection:**

```
Current (synchronous, blocking):
  operator ──BuildReplica RPC──► primary (blocks for hours) ──► response
  operator ──WaitForCatchUp RPC──► primary (blocks forever) ──► response

Better (async, progress-polled):
  operator ──BuildReplica RPC──► primary (returns immediately)
  operator ──WaitForCatchUp RPC──► not needed (poll instead)

  Operator polls GetStatus on primary every 5s:
    build_progress: { state: Copying, lsn: 5000, items: 12000 }
    catch_up_progress: { replica_4_acked: 98, highest: 102, gap: 4 }

  Stall detection (not fixed timeout):
    If progress.lsn unchanged for > stall_timeout (default 30s):
      → stalled or dead — abort, restart_secondary
    If progress.lsn advancing:
      → still working, keep waiting (even if it takes hours)
```

**Progress signals needed:**

| Operation | Progress Field | Source |
|-----------|---------------|--------|
| `build_replica` | Items/bytes copied, current LSN | Primary's `handle_build_replica` |
| `wait_for_catch_up` | Per-replica `acked_lsn` vs `highest_lsn` | `QuorumTracker::replica_acked_lsn` |

**Implementation approach:**
- `BuildReplica` RPC returns immediately with a build ID.
  Primary runs copy in background. Operator polls via `GetStatus`.
- `wait_for_catch_up_quorum` removed from `ReplicaHandle` trait.
  Instead, operator reads catch-up progress from `GetStatus` response
  and determines completion itself.
- **Stall detector** in the reconciler: tracks `last_progress_lsn` and
  `last_progress_time`. If unchanged for `stall_timeout`, treat the
  replica as dead and trigger `restart_secondary`.
- Build-in-progress state persisted in CRD status. On operator restart,
  it sees "replica X build in progress" and resumes polling.

**This subsumes B2 (timeout enforcement) for these operations** — stall
detection is the correct model, not fixed timeouts. Fixed timeouts remain
appropriate for short RPCs (update_epoch, change_role, etc.).

---

These gaps affect reliability under adverse conditions. The system may
hang, leak resources, or fail to recover.

### B0. Replication Stream Failure Goes Undetected

**Severity:** 🔴 Critical
**Affects:** `PrimarySender`, `WalReplicatorActor`, operator reconciler
**Files:** `primary.rs`, `actor.rs`, `reconciler.rs`

**Problem:** The replication stream is a long-lived bidirectional gRPC
stream that lives for the entire lifetime of a secondary replica. Unlike
the copy stream (short-lived, operator retries the build), the replication
stream carries all ongoing writes. When it breaks, **nobody is notified**:

1. The ACK reader task (spawned at `primary.rs:87`) logs a warning and
   exits silently. No callback, no channel notification.
2. The dead `item_tx` stays in `PrimarySender::connections`. `send_to_all()`
   tries to send, gets `Err` (receiver dropped), logs a warning. Does NOT
   remove the connection or notify anyone.
3. The secondary stops ACKing. With 3 replicas (quorum=2), writes still
   succeed via primary + 1 remaining secondary. But if a second stream
   breaks, pending `replicate()` calls hang forever — `QuorumTracker`
   has no per-operation timeout.
4. The operator reconciler is blind to this. It checks pod readiness
   (`is_pod_ready`), not replication stream health. A pod can be Ready
   (process running, kubelet OK) but its replication stream dead.

**Failure timeline:**
```
T0: Replication stream to Replica 3 breaks (network blip)
    ACK reader exits. item_tx still in connections.
    PrimarySender logs warning on each send_to_all(), keeps going.
    QuorumTracker stops receiving ACKs from Replica 3.
    Writes still succeed (quorum = primary + Replica 2 = 2).

T1: Replication stream to Replica 2 also breaks
    Now: quorum = primary only = 1 < write_quorum(2)
    All pending replicate() calls hang forever.
    No timeout. No notification to operator.
    User sees: replicate() never returns.

T2: ??? — operator never detects this.
    Pods are Ready. No failover triggered. System is frozen.
```

**Design needed:**

- **Dead connection detection + cleanup:** When the ACK reader task
  exits (stream error or EOF), it should:
  1. Remove the connection from `PrimarySender::connections`
  2. Notify the replicator actor (e.g., via a `mpsc::Sender` for
     connection-death events)
  3. The actor can then update `QuorumTracker` (remove replica from
     active set) and surface `NoWriteQuorum` if quorum is now lost.

- **Operator-visible health signal:** The replicator should expose
  replication health via `GetStatus` RPC (or `ReplicaInfo`). The
  reconciler can then detect "pod Ready but replication broken" and
  trigger `restart_secondary`.

- **Per-operation timeout in QuorumTracker:** `register()` should
  accept a timeout. If quorum ACK doesn't arrive within the timeout,
  fail the pending `replicate()` call with `NoWriteQuorum` instead of
  hanging forever.

- **Automatic removal of dead connections in `send_to_all()`:**
  Currently logs a warning and keeps the dead entry. Should remove it:
  ```rust
  let mut dead = Vec::new();
  for (rid, conn) in &self.connections {
      if conn.item_tx.send(item.clone()).await.is_err() {
          dead.push(*rid);
      }
  }
  for rid in dead {
      self.connections.remove(&rid);
  }
  ```

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
| Operator restart recovery (`PartitionDriver::recover()`) | operator-failure-scenarios.md §8 | P0 |
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
| A: Protocol Safety (needs design) | 5 | **A1 ✅ fixed**, **A2 ✅ fixed**, **A5 async build_replica** |
| B: Operational Resilience (needs design) | 5 | **B0 replication stream death**, B2 timeouts |
| C: Correctness Refinements (needs design) | 4 | **C0 ✅ fixed**, C1 stale ACKs |
| D: Already Designed (implement only) | 14 | Data loss protocol, operator restart |
| **Total** | **28** | |

**Recommended order:**
1. **B0 (replication stream death)** — silent data path failure, critical
2. **A5 (async build + catch-up with stall detection)** — blocks reconciler, breaks on large datasets
3. A4 (gRPC control plane ordering) — foundational safety
4. B2 (short RPC timeouts) — prevents hangs on control-plane calls
5. A3 (promotion failure) — failover resilience
6. D items P0 (data loss, operator restart, secondary health)
7. B3 + B4 (reconnection, operation locking) — operational maturity
8. B1 streaming improvement — large dataset support
9. C1-C3 (refinements) — can be done alongside other work
