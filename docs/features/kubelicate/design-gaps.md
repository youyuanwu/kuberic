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

### A1. Partial Failure in Multi-Replica Operations

**Severity:** 🔴 Critical
**Affects:** `failover()`, `switchover()`, `create_partition()`, `add_replica()`
**File:** `kubelicate-core/src/driver.rs`

**Problem:** All multi-replica operations loop with `?` (fail-on-first-error).
If `update_epoch` succeeds on replica 2 but fails on replica 3, the system
is in an inconsistent state — some secondaries fenced with the new epoch,
others still in the old epoch. No rollback, no retry-remaining.

**Example:**
```
Failover starts, new_epoch = (0, 2)
Replica 2: update_epoch → SUCCESS (now in epoch 2)
Replica 3: update_epoch → TIMEOUT (still in epoch 1)
Function returns Err → driver state unchanged
Result: Replica 2 in epoch (0,2), Replica 3 in epoch (0,1)
Both could accept writes from different primaries
```

**Design needed:**
- Retry-all semantics: on partial failure, retry failed replicas (not
  start over). `update_epoch` is idempotent — safe to retry.
- Track per-replica epoch state in the driver (which replicas were
  successfully fenced).
- Define "minimum fencing quorum": how many secondaries must be fenced
  before promotion is safe? (Answer: write quorum - 1, since primary
  counts toward quorum.)
- If minimum fencing quorum not achievable, abort failover and report error.

---

### A2. Switchover Write Revocation Missing

**Severity:** 🔴 Critical
**Affects:** `switchover()`
**File:** `kubelicate-core/src/driver.rs`

**Problem:** Design specifies staged revocation: "write revoked first
(step 2), reads later (step 4)". Implementation jumps directly to
`change_role(ActiveSecondary)`. During the gap between epoch fencing
and demotion, the old primary can still accept and commit writes that
the new primary won't have.

**SF reference:** SF's SwapPrimary has an explicit Phase 0 (Demote) with
double-catchup — first catchup WITH writes still flowing, then write
revocation, then second catchup WITHOUT writes, then activate.

**Design needed:**
- Two-phase revocation protocol:
  1. `set_write_status(ReconfigurationPending)` on old primary
  2. Wait for in-flight `replicate()` calls to drain (with timeout)
  3. Verify: old primary's committed LSN matches what secondaries have
  4. Then demote via `change_role(ActiveSecondary)`
- How to implement "wait for drain": the old primary's
  `StateReplicatorHandle` should reject new `replicate()` calls, and
  existing pending operations should complete (or timeout).
- Currently `PartitionState` has `write_status` as an atomic — we can
  set it to `ReconfigurationPending` from the driver. But there's no
  RPC for this (the `ReplicaHandle` trait has no `revoke_write` method).

**New RPC needed:** `RevokeWriteStatus` — tells the pod to set
`write_status = ReconfigurationPending` and drain pending writes.

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

### C0. Build Buffer Replay Test Flaky Under Parallel Execution

**Severity:** 🟢 Low (test infrastructure issue, not a design flaw)
**Affects:** `test_operator_build_buffer_replay`
**File:** `examples/kv-stateful/src/tests.rs`

**Problem:** The build buffer replay test writes concurrently during
`add_replica`. Under parallel test execution (17 tests, each with
multiple pods and gRPC servers), CPU contention causes the tokio runtime
to not schedule tasks fast enough. The `reply_timeout` (5s) expires on
state provider callbacks, not because of channel contention, but because
the runtime is saturated.

**Analysis:** The user's event loop (in `service.rs`) is free — replication
streams are drained in spawned tasks, not in the select loop. The
`state_provider_tx` channel (capacity 16) is only written to by PodRuntime
on the primary side. The actual bottleneck is CPU scheduling under heavy
parallel test load.

**Not a design flaw:** In SF, `IStateProvider` methods are direct calls
on the user's object — but our channel-based approach is equivalent when
the runtime has sufficient CPU. The channel capacity (16) is adequate
since events are processed quickly by the user's select loop.

**Status:** Test marked `#[ignore]` — passes reliably when run alone.
Can be un-ignored once tests are configured with limited parallelism
(e.g., `cargo test -j 1`) or when the test uses a dedicated
multi-threaded runtime.

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
| A: Protocol Safety (needs design) | 5 | A1 partial failure, **A5 async build_replica** |
| B: Operational Resilience (needs design) | 5 | **B0 replication stream death**, B2 timeouts |
| C: Correctness Refinements (needs design) | 4 | C1 stale ACKs (quick fix) |
| D: Already Designed (implement only) | 14 | Data loss protocol, operator restart |
| **Total** | **28** | |

**Recommended order:**
1. **B0 (replication stream death)** — silent data path failure, critical
2. **A5 (async build + catch-up with stall detection)** — blocks reconciler, breaks on large datasets
3. A1 + A4 (partial failure + ordering) — foundational safety
4. B2 (short RPC timeouts) — prevents hangs on control-plane calls
5. A2 (write revocation) — switchover correctness
6. A3 (promotion failure) — failover resilience
7. D items P0 (data loss, operator restart, secondary health)
8. B3 + B4 (reconnection, operation locking) — operational maturity
9. B1 streaming improvement — large dataset support
10. C1-C3 (refinements) — can be done alongside other work
