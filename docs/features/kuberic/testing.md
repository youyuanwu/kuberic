# Kuberic: Test Strategy

How the Kuberic project is tested — test layers, infrastructure,
what each layer validates, and known gaps.

> Part of the [Kuberic Design](../kuberic-replicator-design.md).

---

## Test Layers

The project uses three testing layers, each with different scope and
fidelity. Higher layers exercise more integration but are slower and
harder to debug.

```
Layer 3: Reconciler E2E + Operator-driven integration tests
         └─ KvClusterApi/KvPod → real PodRuntime + KV service per pod
            Full reconciler state machine, real gRPC, real replication

Layer 2: Driver unit tests
         └─ PartitionDriver + InProcessReplicaHandle (no app)
            Protocol correctness: failover, switchover, epoch fencing

Layer 1: Component unit tests
         └─ QuorumTracker, NoopReplicator, KubericRuntime, PodRuntime
            Individual component behavior in isolation
```

---

## Layer 1: Component Unit Tests

Test individual components in isolation. Fast, deterministic.

### QuorumTracker (`replicator/quorum.rs` — 7 tests)

| Test | What It Validates |
|------|-------------------|
| `test_single_replica_commits_immediately` | Primary alone satisfies quorum=1 |
| `test_three_replicas_quorum` | 3-replica set, quorum=2, commit on 2nd ACK |
| `test_dual_config_quorum` | During reconfig: must satisfy BOTH CC and PC quorum |
| `test_out_of_order_acks` | ACKs arriving for higher LSN before lower LSN |
| `test_fail_all` | Role change / close fails all pending operations |
| `test_must_catch_up_enforcement` | Write mode: specific replica must individually ACK |
| `test_wait_catch_up_all_mode` | All mode: every member must ACK |

**Infrastructure:** Direct `QuorumTracker` construction, no actors or gRPC.

### NoopReplicator (`noop.rs` — 3 tests)

| Test | What It Validates |
|------|-------------------|
| `test_noop_lifecycle` | Open → ChangeRole → Close lifecycle |
| `test_noop_replicate_handle` | StateReplicatorHandle::replicate() works |
| `test_noop_replicate_not_primary` | replicate() before promotion returns NotPrimary |

**Infrastructure:** `KubericRuntime` with `NoopReplicator` (no quorum,
no gRPC). Tests the event loop and handle APIs.

### KubericRuntime (`runtime.rs` — 3 tests)

| Test | What It Validates |
|------|-------------------|
| `test_runtime_full_lifecycle` | Full lifecycle with real `WalReplicatorActor` |
| `test_runtime_replicate_before_promote` | replicate() blocked until Primary role |
| `test_runtime_abort` | Abort event stops the runtime |

**Infrastructure:** `KubericRuntime` with `WalReplicatorActor` (real
quorum tracking, no gRPC).

### PodRuntime (`pod.rs` — 1 test)

| Test | What It Validates |
|------|-------------------|
| `test_pod_runtime_user_lifecycle` | Open → ChangeRole(Primary) → replicate → demote → Close via gRPC control server |

**Infrastructure:** `PodRuntime::builder()` with real gRPC servers. Tests
the dual-channel event delivery (lifecycle + state_provider) and the
command routing from gRPC → PodRuntime → replicator + user.

---

## Layer 2: Driver Unit Tests

The driver tests were migrated to KvPod-based integration tests (Layer 3).
The `InProcessReplicaHandle` and `PartitionDriver` testing infrastructure
remains available for future use (e.g., testing new driver methods before
wiring the full KV stack).

### InProcessReplicaHandle

Available for tests that need fast, no-app protocol testing. Each
`InProcessReplicaHandle` spawns:
- A `WalReplicatorActor` (real quorum + replication logic)
- A `SecondaryReceiver` gRPC data server (real ACK handling)
- Channels for control and data events

```rust
let handle = InProcessReplicaHandle::spawn(id).await?;
```

It implements `ReplicaHandle` via in-process channels (no network), so
the driver can orchestrate protocols at full speed. The data plane uses
real gRPC (localhost) because `PrimarySender` connects to secondaries
via gRPC streams.

**Note:** `InProcessReplicaHandle`'s `build_replica` is a no-op — it
calls `PrimarySender::start_build()` (starts build buffering) but does
NOT execute the copy protocol. The full copy flow requires `PodRuntime`
(which `KvPod` provides). This is why driver tests were migrated.

---

## Layer 3: Integration & E2E Tests

Test the full stack: `PartitionDriver` with `GrpcReplicaHandle` driving
real `PodRuntime` pods running the KV store application. Real gRPC
transport, real copy/replication streams, real user state management.
Also tests the reconciler state machine end-to-end.

### Operator-Driven Tests

**File:** `examples/kvstore/src/tests.rs` — 8 tests

| Test | What It Validates |
|------|-------------------|
| `test_operator_single_replica_kv` | Single pod: create, write KV via client gRPC, read back. |
| `test_operator_three_replica_failover` | 3 pods, write data, failover, verify data survives on new primary. |
| `test_operator_kv_crud_operations` | Put/Get/Delete operations, verify consistency across replicas. |
| `test_operator_restart_secondary_copies_state` | restart_secondary: close + remove + add_replica. Verify copy protocol delivers full state to rebuilt secondary. |
| `test_operator_scale_up` | Scale 1→3: add two replicas, verify copy delivers existing data. |
| `test_operator_scale_down` | Scale 3→1: remove two replicas, verify writes still work. |
| `test_operator_switchover` | Switchover: old primary rejects writes, new primary works, data survives. |
| `test_operator_build_buffer_replay` | Write 500 entries, then add a 4th replica while writing 200 more concurrently. Verifies ReplicationQueue replay delivers all ops to the new secondary. 700 total entries verified. |
| `test_operator_epoch_fencing_after_failover` | Failover: new primary works, pre-failover data survives on new primary. |
| `test_operator_delete_partition` | delete_partition closes all replicas. Primary rejects writes after deletion. |
| `test_operator_secondary_state_after_failover` | After failover, secondaries retain all committed data (epoch truncation preserves committed ops). |

### Reconciler E2E Tests

**File:** `examples/kvstore/src/reconciler_tests.rs` — 4 tests

Test the full reconciler state machine driving real pods. `KvClusterApi`
implements `ClusterApi` by spawning real `PodRuntime` + KV service pods.
Also supports `mark_pod_not_ready()` for testing failure detection paths.

| Test | What It Validates |
|------|-------------------|
| `test_reconciler_creates_partition_and_serves_kv` | Full Pending→Creating→Healthy flow. Write KV data, read from another pod. |
| `test_reconciler_switchover` | Switchover via targetPrimary change. Verify old primary rejects writes. |
| `test_reconciler_creating_waits_for_ready` | Creating phase requeues when pods are not ready (no transition to Healthy). |
| `test_reconciler_detects_primary_failure_and_fails_over` | Healthy detects NotReady primary → FailingOver → failover completes → Healthy with new primary. Verifies pre-crash data survives and new primary accepts writes. |
| `test_reconciler_scale_up` | Healthy phase: spec.replicas increased → creates pods → adds replicas to driver. |
| `test_reconciler_scale_down` | Healthy phase: spec.replicas decreased → removes secondary from driver. |

### KvPod Helper

Each test spins up `KvPod` instances — a real `PodRuntime` + KV service
event loop + client gRPC server:

```rust
let pod = KvPod::start(id).await;
let handle = pod.replica_handle(id).await;  // GrpcReplicaHandle
let client = connect_kv_client(&pod.client_address).await;
```

### KvClusterApi

Implements `ClusterApi` trait. Instead of creating K8s pods, it spawns
local `KvPod` instances. Also provides `mark_pod_not_ready()` for
testing failure detection paths with real pods:

```rust
impl KvClusterApi {
    fn mark_all_pods_ready(&self) { ... }
    fn mark_pod_not_ready(&self, pod_name: &str) { ... }
}

impl ClusterApi for KvClusterApi {
    async fn create_pod(&self, ...) -> Result<Pod> {
        // Spawns real PodRuntime + KV service
    }
    async fn create_replica_handle(&self, ...) -> Result<Box<dyn ReplicaHandle>> {
        // Returns GrpcReplicaHandle connected to the live pod
    }
}
```

---

## Test Infrastructure Summary

| Component | Purpose | Used By |
|-----------|---------|--------|
| `QuorumTracker` (direct) | Test quorum math in isolation | Layer 1 |
| `NoopReplicator` | Stub replicator for lifecycle tests | Layer 1 |
| `KubericRuntime` | Lower-level harness (no gRPC) | Layer 1 |
| `InProcessReplicaHandle` | Real replicator + gRPC data server, in-process control | Available (Layer 2) |
| `KvPod` | Real PodRuntime + KV service + client server | Layer 3 |
| `GrpcReplicaHandle` | Real gRPC transport to pods | Layer 3 |
| `KvClusterApi` | Mock ClusterApi backed by real KvPods + readiness control | Layer 3 (reconciler) |

---

## How to Run Tests

```bash
# All tests
cargo test -p kuberic-core -p kvstore

# Core crate only
cargo test -p kuberic-core

# KV example only
cargo test -p kvstore

# Specific test
cargo test test_driver_failover

# With logging (requires test-log crate)
RUST_LOG=info cargo test test_operator_three_replica_failover -- --nocapture
```

---

## What's Tested vs What's Not

### Well-Tested (Happy Paths)

- ✅ Full create → write → failover → write lifecycle
- ✅ Switchover with old-primary write rejection
- ✅ Scale-up with copy protocol (full state transfer)
- ✅ Scale-down with config-first removal
- ✅ Restart secondary with rebuild
- ✅ Dual-config quorum during reconfiguration
- ✅ must_catch_up enforcement
- ✅ Catch-up baseline (no false catches on historical ops)
- ✅ Reconciler state machine (Pending→Creating→Healthy→FailingOver→Switchover)
- ✅ Reconciler: Creating waits for pod readiness
- ✅ Reconciler: Healthy detects NotReady primary → full failover cycle

### Not Tested (Implemented but Untested Code Paths)

| Gap | What's Missing | Difficulty |
|-----|---------------|------------|
| `on_data_loss` callback | Never triggered — actor hardcodes `DataLossAction::None`. Need quorum loss scenario. | Medium (needs quorum loss sim) |
| `remove_replica` (cancel build) | No test cancels an in-progress `build_replica` via `remove_replica`. | Medium |
| Multiple sequential operations | Failover→failover, switchover→failover, scale-up→failover. Only single operations tested. | Medium |

### Not Tested (Requires Design Work First)

| Gap | Category | Design Gap Reference |
|-----|----------|---------------------|
| Partial update_epoch failure (some replicas fenced, others not) | Protocol safety | A1 |
| Promotion failure after fencing | Protocol safety | A3 |
| gRPC ordering violations | Protocol safety | A4 |
| Replication stream death mid-operation | Operational | B0 |
| Build/catch-up stall detection | Operational | A5 |
| gRPC handle reconnection after pod restart | Operational | B3 |
| Concurrent reconciliation (race conditions) | Operational | B4 |
| QuorumTracker stale ACK cleanup | Correctness | C1 |
| Zombie primary write rejection (epoch fencing on data plane) | Protocol safety | A2 |
| Data loss protocol (on_data_loss triggered by operator) | Designed, not impl | D |
| Operator restart recovery | Designed, not impl | D |
| Secondary health detection | Designed, not impl | D |
| Missing pod detection | Designed, not impl | D |
| Network partition (pod Ready but gRPC unreachable) | Designed, not impl | D |

### Intentionally Not Tested

- **Real Kubernetes integration** — requires a cluster. Future work:
  kind/minikube-based integration tests.
- **mTLS** — deferred to post-MVP.
- **Large dataset copy** — in-memory state, no multi-GB test fixtures.
- **Performance/latency** — no benchmarks yet. The atomic status reads
  (`PartitionState`) are designed for ~1ns but not benchmarked.

---

## Testing Principles

1. **Layer 2 (driver tests) is the primary correctness layer.** Protocol
   invariants (fence-before-promote, config-first removal, dual-config
   quorum) are tested here because `InProcessReplicaHandle` exercises
   the real replicator logic without gRPC noise.

2. **Layer 3 integration tests validate the full stack.** These tests
   catch integration issues (gRPC serialization, stream lifecycle,
   copy protocol end-to-end) that Layer 2 can't. Both operator-driven
   (direct driver use) and reconciler-driven (state machine) variants
   exercise the same underlying protocols.

3. **`InProcessReplicaHandle` is the key abstraction.** It enables driver
   tests to run fast (no network) while still exercising real quorum
   tracking, real ACK handling, and real epoch fencing. The
   `ReplicaHandle` trait makes this possible — same driver code works
   with in-process channels (tests) or gRPC (production).

4. **No separate gRPC transport tests.** gRPC transport correctness is
   validated implicitly by Layer 3 tests which use real `GrpcReplicaHandle`
   + real `PodRuntime`. Dedicated gRPC-only tests were removed as they
   covered a strict subset of Layer 3.

5. **Error path testing is the main gap.** Happy paths are well-covered
   across all 3 layers. Error paths (partial failures, stream deaths,
   timeouts, concurrent operations) are almost entirely untested. This
   mirrors the design gaps — error handling design is needed before
   error tests can be written.

---

## Simulating Pod Crash and Restart

Testing crash and restart scenarios requires different strategies at
different test layers. The key challenge: what state should a restarted
pod have?

### Current Crash Simulation Methods

| Method | Fidelity | Used In |
|--------|----------|---------|
| `handle.close()` | Clean shutdown — sends Close to PodRuntime, drains gracefully. Not a real crash. | operator_failover tests |
| `remove_replica_from_driver(id)` | Removes handle from driver tracking. Simulates operator detecting dead pod. Pod process may still be running. | failover-with-secondary-down test |
| `mark_pod_not_ready(name)` | Sets pod readiness to false. Reconciler detects and triggers failover. Pod still running. | reconciler tests |
| `drop(handle)` | Drops the ReplicaHandle. gRPC connections close. PodRuntime sees stream errors and eventually stops. Closest to a crash. | Not commonly used |

**Limitation:** None of these simulate a real crash-restart cycle where
the pod comes back with fresh state. `restart_secondary()` does this
for secondaries, but there's no equivalent for testing primary restart.

### Design: Crash-Restart Simulation

A proper crash-restart test needs to model the full lifecycle:

```
1. Pod is running (has state, replicator active, gRPC connected)
2. Pod crashes (in-memory state lost — handles closed, gRPC streams broken)
3. Time passes (operator detects, may trigger failover)
4. Pod restarts (fresh PodRuntime, recovers from checkpoint, new gRPC addresses)
5. Operator re-integrates (either as new secondary via add_replica,
   or if primary crashed, failover happened and this rejoins as secondary)
```

#### KvPod::crash() — Abrupt Termination

A `crash()` method that simulates sudden process death:

```rust
impl KvPod {
    /// Simulate a pod crash. Aborts the PodRuntime and service without
    /// graceful shutdown. All in-memory state is lost. gRPC connections
    /// break with transport errors. The KvPod instance becomes unusable.
    pub async fn crash(self) {
        // Abort the runtime (no Close event, no drain)
        self._runtime_handle.abort();
        self._service_handle.abort();
        // Drop all handles — gRPC channels break immediately
        // State is dropped (simulating memory loss)
    }
}
```

Unlike `close()` (which sends Close events and waits for drain),
`crash()` calls `abort()` — tasks are killed immediately. The driver's
`GrpcReplicaHandle` will get transport errors on next RPC.

#### KvPod::restart() — Fresh Pod on Same ID

A restart creates a brand new PodRuntime with empty state, as if K8s
restarted the container:

```rust
impl KvPod {
    /// Crash this pod and start a fresh one with the same replica ID.
    /// Returns new KvPod that recovers from checkpoint (same data_dir).
    pub async fn restart(self, id: i64) -> KvPod {
        let dir = self.data_dir.clone();
        self.crash().await;
        // Small delay to simulate restart time
        tokio::time::sleep(Duration::from_millis(100)).await;
        // Same data_dir = recovers from checkpoint + WAL
        KvPod::start_with_dir(id, dir, Duration::from_secs(5)).await
    }
}
```

**Important:** The new pod has different gRPC addresses (new random
ports). The operator must create a new `GrpcReplicaHandle` for the
restarted pod — the old handle is dead.

#### What State Should a Restarted Pod Have?

**Current (checkpoint-only persistence):** The KV service recovers
from the last checkpoint (snapshot + WAL replay). State from graceful
shutdown or copy completion is preserved. State from a crash between
checkpoints is lost (no per-op WAL yet — see `kvstore-design.md`
Implementation Status for the deadlock blocker).

**With per-op WAL (future):** All acknowledged ops survive crashes.
The PodRuntime reads its WAL on startup and recovers `last_lsn` and
`epoch`. Reports these to the operator via `GetStatus`. Requires
fixing the RwLock + async I/O deadlock first.

#### Test Patterns for Crash-Restart

**Pattern 1: Secondary crash-restart (existing)**

Already implemented via `restart_secondary()`:
```rust
// Close old handle, create new KvPod, add_replica
let new_pod = KvPod::start(old_id).await;
let new_handle = new_pod.replica_handle(old_id).await;
driver.restart_secondary(old_id, Box::new(new_handle)).await?;
// → Primary sends copy stream → new secondary rebuilds state
```

**Pattern 2: Primary crash → failover → old primary rejoins**

```rust
// 1. Crash the primary
let primary_pod = pods.remove(&primary_id);
primary_pod.crash().await;

// 2. Remove dead handle from driver (operator detects)
driver.remove_replica_from_driver(primary_id);

// 3. Failover to a secondary
driver.failover().await?;
let new_primary_id = driver.primary_id().unwrap();

// 4. Old primary restarts as fresh pod
let restarted = KvPod::start(primary_id).await;
let handle = restarted.replica_handle(primary_id).await;

// 5. Re-add as secondary (gets full copy from new primary)
driver.add_replica(Box::new(handle)).await?;

// 6. Verify: old primary's data rebuilt via copy protocol
wait_for_state_count(&restarted.state, expected_count).await;
```

**Pattern 3: Primary crash → failover → verify no data loss**

```rust
// Write data, crash primary, failover, verify all committed
// data survives on new primary. (test_reconciler_double_failover
// already does this via mark_pod_not_ready)
```

**Pattern 4: Simultaneous crash (quorum loss)**

```rust
// Crash 2 of 3 pods → quorum lost → writes should hang/fail
// (Requires QuorumTracker timeout — gap B0)
```

**Pattern 5: Crash during switchover (A3 rollback)**

```rust
// Start switchover, crash target before promotion completes
// → Switchover should rollback, re-promote old primary
// (test_switchover_rollback_on_target_failure already tests this
//  via handle.close(), but crash() is more realistic)
```

#### Driver and Reconciler Integration

**For driver-level tests:** The test manually calls `crash()`, then
creates a new handle and calls `restart_secondary()` or `add_replica()`.

**For reconciler-level tests:** The reconciler needs to detect the
crashed pod (via `mark_pod_not_ready()` or `get_pod_status()`) and
handle re-integration automatically. The `KvClusterApi` should:
1. Detect crash via readiness check
2. Delete old pod (K8s garbage collection equivalent)
3. Create new pod (fresh KvPod)
4. Create new handle (fresh GrpcReplicaHandle)

This maps to reconciler states:
```
Healthy → pod crashes
  → Reconciler detects NotReady
  → If primary: FailingOver → Healthy (new primary)
  → If secondary: Healthy (remove + re-add in same cycle)
```

#### Implementation Priority

1. **`KvPod::crash()`** — minimal, unblocks Pattern 2-5 tests
2. **Pattern 2 test** — primary crash → failover → rejoin
3. **`KvPod::restart()`** convenience wrapper
4. **Pattern 4** — quorum loss (blocked on B0 QuorumTracker timeout)
5. **WAL recovery tests** — blocked on Option C implementation
