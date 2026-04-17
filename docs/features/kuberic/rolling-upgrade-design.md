# Rolling Upgrade Design

Rolling upgrades for kuberic-managed stateful partitions. Supports
application image updates, configuration changes, and PG major version
upgrades — all with zero or minimal downtime.

---

## Goals

1. Rolling upgrade of application pods (image change) with zero downtime
2. Configuration changes applied without full rebuild
3. Primary upgrade via switchover (no downtime) or restart (brief downtime)
4. Operator-driven: user updates the CRD spec, operator orchestrates

## Non-Goals

- Live binary patching of the kuberic runtime itself (pod restart required)
- Cross-partition upgrades (each partition upgraded independently)
- Blue-green or canary deployment strategies (rolling only)
- Automatic rollback on upgrade failure (manual intervention)

---

## Background: How CNPG Does It

CNPG has a mature rolling update system
(`internal/controller/cluster_upgrade.go`) worth learning from.

### Detection (`isInstanceNeedingRollout`)

CNPG runs sequential checkers — returns on first match:

1. **Pod must be ready** (`IsPodReady == true`) — skip unready pods
2. **Missing PVCs** — `checkHasMissingPVCs()`
3. **Projected volume outdated** — ConfigMaps/Secrets changed
4. **Container image outdated** — `cluster.Status.Image` vs pod image
5. **Restart annotation mismatch** — user-triggered via
   `cnpg.io/restartCluster` annotation
6. **PodSpec annotation outdated** — full PodSpec hash comparison
7. **Legacy checks** (if no PodSpec annotation): env vars, scheduler,
   topology constraints, bootstrap container image
8. **Pending restart** — PG config change needs restart (sets
   `canBeInPlace = true` — only this path allows in-place restart)

### Execution Order

**Replicas first (most-lagged first) → Primary last.**

For each replica needing update:
1. Check rollout rate limiter (`rolloutManager.CoordinateRollout`)
2. If allowed: delete the pod
3. CNPG's reconciler detects missing pod, creates new pod directly
   (NOT via StatefulSet — CNPG manages pods directly with
   `ctrl.SetControllerReference`). New pod binds to existing PVC
   by name matching.
4. Wait for recreated pod to rejoin replication
5. Proceed to next replica

### Primary Update

Two orthogonal settings:

**Primary Update Strategy** (when to proceed):
- `unsupervised` (default): Automatic — upgrades primary after replicas
- `supervised`: Pauses after replicas, sets `PhaseWaitingForUser`.
  User must manually trigger switchover. Does NOT consume a rollout
  slot while waiting.

**Primary Update Method** (how to upgrade):
- `switchover`: Selects `podList.Items[1]` (least-lagged replica).
  Verifies WAL receiver is active (streaming, not log-shipping). If
  elected replica is log-shipping: returns `errLogShippingReplicaElected`,
  blocks upgrade until streaming replica available. **No retry, no
  rollback on switchover failure** — error propagates to reconciler.
- `restart`: In-place if `canBeInPlace` (config-only change, sets
  annotation for instance manager to restart PG). Delete-recreate if
  image changed.

### In-Place vs Delete-Recreate

| Condition | Action |
|-----------|--------|
| Config-only change (`PendingRestart = true`) | In-place: set annotation, instance manager restarts PG |
| Image change | Delete pod, recreate with new image. PVC preserved |
| Image + config change | Delete pod (can't do in-place with image change) |

### Hot Standby-Sensitive Parameters

CNPG has a **hardcoded list** of 5 parameters that require special
handling when *decreased*:
- `max_connections`, `max_prepared_transactions`, `max_wal_senders`,
  `max_worker_processes`, `max_locks_per_transaction`

When decreased: primary must restart first, then replicas pick up new
values from `pg_controldata`. CNPG queries `pg_settings` table to detect
which pending changes are decreases of these parameters
(`PendingRestartForDecrease` flag).

### Instance Manager Online Update

CNPG can update the instance manager binary (Go process, PID 1 inside
pod) without restarting the pod or PG. Compares `ExecutableHash` per
pod, pushes new binary via HTTP. Instance manager hot-swaps itself.
Separate from pod rolling update — can happen while PG is serving
traffic.

### Rate Limiting

Global rollout coordinator (`internal/controller/rollout/rollout.go`):
- Single global slot across ALL clusters
- One pod rollout at a time
- Configurable `clusterRolloutDelay` (between clusters) and
  `instanceRolloutDelay` (between instances in same cluster)
- If delay not elapsed: return `errRolloutDelayed`, reconciler requeues

### Health Between Restarts

CNPG has **no explicit health check** between replica upgrades. It
relies on:
1. Pod readiness probes (Kubernetes)
2. Instance manager `/status` endpoint reporting
3. WAL receiver active check (before switchover only)
4. Config uniformity wait (`LoadedConfigurationHash` across pods)

### Major Version Upgrade

CNPG supports in-place PG major version upgrades via a Kubernetes Job
that runs `pg_upgrade` on the primary's PVC. PVCs are preserved,
timeline resets to 1 after upgrade. Replicas rebuilt via
`pg_basebackup` after primary upgrade.

---

## Background: How Service Fabric Does It

SF has a sophisticated upgrade system built around **Upgrade Domains**
(UD) — a concept that maps well to Kubernetes node groups but operates
at a different abstraction level.

### Upgrade Domains

SF partitions cluster nodes into upgrade domains — logical groups that
control which nodes can upgrade simultaneously. Upgrades proceed **one
UD at a time**, sequentially. All nodes in UD[N] must complete before
advancing to UD[N+1].

Kuberic equivalent: Since kuberic manages individual pods (not nodes),
the "upgrade domain" is effectively a single pod — we upgrade one pod
at a time within a partition.

### Upgrade Modes

| Mode | Behavior | Kuberic Equivalent |
|------|----------|--------------------|
| **UnmonitoredAuto** | Auto-advance between UDs, no health checks | `unsupervised` strategy |
| **UnmonitoredManual** | Pause after each UD, operator must continue | `supervised` strategy |
| **Monitored** | External health evaluation between UDs | Future: health gate between pod upgrades |

### Safety Checks Before Upgrade

SF performs safety checks on every replica before allowing a node to
upgrade (`UpgradeSafetyCheckKind` enum):

| Check | What It Does | Kuberic Equivalent |
|-------|-------------|-------------------|
| `EnsurePartitionQuorum` | Verify quorum maintained after replica removal | Check `write_quorum` before removing secondary |
| `WaitForPrimarySwap` | Wait for primary to move off this node | `switchover()` before upgrading primary's pod |
| `WaitForReconfiguration` | Wait for in-flight reconfigurations | Wait for `UpdateCatchUpConfiguration` to complete |
| `WaitForInBuildReplica` | Wait for replica build to complete | Wait for `add_replica` / `BuildReplica` to finish |
| `EnsureSeedNodeQuorum` | Fabric upgrade: seed node majority | N/A (no seed nodes in K8s) |
| `EnsureAvailability` | Don't close primary during quorum loss | Block upgrade if partition unhealthy |

**Key insight**: SF blocks node upgrades until ALL safety checks pass.
This is more conservative than CNPG, which just deletes the pod and
lets the reconciler handle it.

### Replica Movement During Upgrade

SF's approach to stateful service replicas during upgrade:

1. **Secondaries on the upgrading node**: Closed (stopped), restarted
   when node comes back up. No movement to other nodes.
2. **Primary on the upgrading node**: **Must be swapped** to a different
   node before the upgrade proceeds. Uses the `SwapPrimary`
   reconfiguration (same as kuberic's `switchover()`):
   - Phase 0: Demote old primary (revoke writes)
   - Double catchup (with and without write status)
   - Phase 4: Activate new primary on different node
3. **Quorum is maintained throughout** — SF blocks the upgrade if
   removing the node's replicas would break quorum.

### Node Upgrade Phases

Each node progresses through:
```
PreUpgradeSafetyCheck → Upgrading → PostUpgradeSafetyCheck
```

The node is blocked at `PreUpgradeSafetyCheck` until all safety checks
pass (primary swapped, quorum verified, no in-flight builds). Only then
does the actual upgrade (code/config deployment) proceed.

### Parallel Upgrade Control

`MaxParallelNodeUpgradeCount` limits simultaneous node upgrades within a
single UD. Default is 0 (unlimited within the UD). When set, SF prunes
the ready-nodes list to respect the limit.

### Rollback

SF maintains the previous application version. Rollback is explicit —
initiated via a new upgrade request targeting the previous version. Not
automatic on failure. The FM tracks `rollback_` version for each
application.

### Key SF Lessons for Kuberic

1. **Safety-first**: SF blocks upgrades until quorum is proven safe.
   Kuberic should verify `write_quorum` is maintainable before removing
   a secondary.
2. **Primary movement is mandatory**: SF always swaps primary off the
   upgrading node before proceeding. Kuberic should always switchover
   before upgrading the primary pod.
3. **Catchup before switchover**: SF's `SwapPrimary` does two catchup
   rounds (with and without write status). Kuberic's `switchover()`
   does NOT implement this yet — **E2 bug** in `design-gaps.md`.
   Fix: poll target `current_progress` after `revoke_write_status`
   before demotion.
4. **Post-upgrade validation**: SF checks health after each node
   upgrade. Kuberic should verify the upgraded pod is healthy and
   replicating before proceeding.
5. **No automatic rollback**: Both SF and CNPG treat rollback as a
   manual decision. Kuberic should follow this pattern — automatic
   rollback for stateful services is risky.

---

## Kuberic Current State

### What Exists

The kuberic operator (`kuberic-operator/src/reconciler.rs`) manages pods
via `PartitionDriver`. The driver supports:

| Operation | Description |
|-----------|-------------|
| `create_partition(replicas)` | Create a new partition with N replicas |
| `failover(dead_id)` | Promote best secondary when primary dies |
| `switchover(target_id)` | Graceful primary swap to target |
| `add_replica(handle)` | Add a new replica to existing partition |
| `remove_secondary(id, min)` | Remove a secondary (config-first, quorum-safe) |
| `restart_secondary(id, new_handle)` | Atomic replace: close old → remove → add new → build |
| `delete_partition()` | Delete the entire partition |

**Key primitive for upgrades**: `restart_secondary(id, new_handle)` is
an atomic one-shot replacement — it closes the old handle, removes the
replica, then calls `add_replica` with the new handle. The new replica
is built from scratch by the primary. This is simpler than separate
remove + delete + create + add steps.

The CRD (`KubericSet`) already has `image`, `replicas`, and port fields
in its spec. Status tracks `phase`, `epoch`, `current_primary`,
`target_primary`, and per-member health/role/LSN.

### What's Missing

1. **No spec drift detection**: The reconciler doesn't compare running
   pod specs against the CRD's desired spec.
2. **No rolling restart loop**: No mechanism to iterate through replicas
   and restart them one-by-one.
3. **No upgrade phase tracking**: No status field to track upgrade
   progress (which pods are upgraded, which are pending).
4. **No rollout coordination**: No rate limiting or delay between restarts.

---

## Design

### Overview

The upgrade workflow adds a reconcile loop to the operator that:
1. Detects spec drift between desired (CRD) and actual (running pods)
2. Upgrades replicas one-by-one (most-lagged first)
3. Upgrades the primary via switchover or restart
4. Tracks progress in CRD status

### Upgrade Triggers

The operator detects an upgrade is needed when the CRD's desired spec
diverges from running pods. Possible triggers:

| Trigger | Detection Method |
|---------|------------------|
| Image change | Compare CRD `.spec.image` vs pod container image |
| Config change | Compare CRD `.spec.config` hash vs pod annotation |
| Resource change | Compare CRD `.spec.resources` vs pod resource limits |
| User-triggered restart | CRD `.spec.restartAnnotation` differs from pod annotation |

### Upgrade Strategy

```
spec:
  upgradeStrategy:
    primaryUpdateStrategy: unsupervised | supervised
    primaryUpdateMethod: switchover | restart
    maxUnavailable: 1  # pods that can be down simultaneously
```

### Upgrade Sequence

Inspired by SF's safety-first approach: verify quorum and health at
every step, never proceed if the partition would lose quorum.

```
1. Operator detects spec drift on ≥1 pods

2. PRE-UPGRADE SAFETY CHECK (SF-inspired):
   - Verify partition is healthy (all replicas reporting)
   - Verify write quorum is satisfied
   - If partition unhealthy: set phase = "UpgradeBlocked", wait
   - Mark partition phase = "Upgrading"

3. For each SECONDARY (most-lagged first):
   a. SAFETY CHECK: verify removing this secondary won't break
      write quorum (remaining replicas ≥ write_quorum)
   b. Delete the old pod
   c. Create new pod with updated spec
   d. Wait for new pod to become ready (gRPC control port)
   e. driver.restart_secondary(id, new_handle)
      └─ Atomic: close old → remove → add_replica → build from primary
   f. POST-UPGRADE CHECK: verify new replica is replicating
      (WaitForCatchUpQuorum — SF's PostUpgradeSafetyCheck equivalent)
   g. Proceed to next secondary

4. When all secondaries upgraded:
   a. If primaryUpdateStrategy == supervised:
      - Set phase = "WaitingForUser"
      - Pause — user must trigger switchover manually
   b. If primaryUpdateStrategy == unsupervised:
      - Continue automatically

5. Upgrade PRIMARY (SF: must swap primary off upgrading node first):
   a. If primaryUpdateMethod == switchover:
      - Pick best upgraded secondary
      - driver.switchover(target_id)
        └─ SF's SwapPrimary: Phase 0 demote → double catchup → Phase 4 activate
      - Old primary is now a secondary
      - Delete old primary pod
      - Create new pod with updated spec
      - driver.restart_secondary(old_primary_id, new_handle)
      - Wait for catch-up
   b. If primaryUpdateMethod == restart:
      - Delete primary pod directly
      - Recreate with new spec
      - Wait for recovery + promotion
      - Brief downtime window

6. Mark partition phase = "Running"
```

**Why `restart_secondary` instead of separate remove+add**: The driver's
`restart_secondary(id, new_handle)` does atomic close → remove → add →
build in one call. This maintains the replica ID mapping (same ID
assigned to the new handle), keeps the code simple, and ensures no
partial state if a step fails.

### Pod Spec Drift Detection

Each pod gets an annotation with the hash of its desired spec:

```
metadata:
  annotations:
    kuberic.io/spec-hash: "sha256:abc123..."
```

On each reconcile cycle, the operator:
1. Computes the desired spec hash from the CRD
2. Compares with each pod's annotation
3. Pods with mismatched hash need upgrading

### Upgrade Progress Tracking

CRD status tracks upgrade progress:

```
status:
  phase: "Upgrading"
  upgrade:
    total: 3
    upgraded: 1
    pending: [2, 3]  # replica IDs still on old spec
    currentTarget: 2  # pod being upgraded now
    startedAt: "2026-04-13T00:00:00Z"
```

### Failure Handling

| Failure | Response |
|---------|----------|
| New pod fails to start | Pause upgrade, set phase = "UpgradeFailed", alert user |
| New pod starts but fails health check | Retry with backoff, then pause |
| Switchover fails | Rollback: re-promote old primary, pause upgrade |
| User cancels | Stop upgrading, leave mixed-version cluster |

**No automatic rollback** — rolling back a database upgrade is risky
(schema migrations may have run). The operator pauses and waits for
human decision. The user can:
- Fix the issue and resume
- Manually roll back to the old image
- Delete and rebuild the failed pod

### Interaction with Existing Primitives

The upgrade workflow composes existing driver operations:

| Upgrade Step | Driver Operation |
|-------------|------------------|
| Replace secondary with upgraded pod | `driver.restart_secondary(id, new_handle)` |
| Switchover for primary upgrade | `driver.switchover(target_id)` |
| Catch-up verification | `WaitForCatchUpQuorum` (inside restart_secondary) |

No new driver primitives needed — upgrade orchestration lives entirely
in the operator's reconcile loop.

---

## Comparison

| Aspect | Service Fabric | CNPG | Kuberic (Proposed) |
|--------|---------------|------|--------------------|
| **Unit of upgrade** | Node (Upgrade Domain) | Pod | Pod |
| **Detection** | Upgrade request with new version | 8+ checkers, annotation-based | Spec hash annotation |
| **Order** | All replicas in UD, primary swapped first | Replicas (most-lagged first) → Primary | Replicas (most-lagged first) → Primary |
| **Safety checks** | 8 checks (quorum, primary swap, build, availability) | Pod ready probe only | Quorum check before removal (proposed) |
| **Replica upgrade** | Close on node, restart when node returns | Delete pod, reconciler recreates | `restart_secondary(id, new_handle)` |
| **Primary handling** | Must swap off node before upgrade | switchover / restart | switchover / restart |
| **Upgrade modes** | Monitored / UnmonitoredAuto / UnmonitoredManual | unsupervised / supervised | unsupervised / supervised |
| **Health between steps** | Post-upgrade safety checks per node | Config uniformity wait, pod ready | Catch-up verification (proposed) |
| **Rate limiting** | `MaxParallelNodeUpgradeCount` per UD | Global coordinator, one at a time | `maxUnavailable` (proposed) |
| **Rollback** | Explicit (new upgrade to old version) | Manual | Manual (pause on failure) |
| **Major version upgrade** | N/A (app framework) | `pg_upgrade` Job | Out of scope (future) |
| **In-place restart** | Yes (service restart without node restart) | Yes (annotation → instance manager) | Not initially |

### Key Architectural Differences

**SF vs Kuberic**: SF operates at the **node** level — an entire upgrade
domain (potentially many nodes) upgrades together, and the FM ensures
primaries are swapped off upgrading nodes first. Kuberic operates at the
**pod** level — each pod is upgraded individually, which is simpler but
means we need per-pod safety checks instead of per-UD batch checks.

**CNPG vs Kuberic**: CNPG manages pods directly (no StatefulSet) — same
as kuberic. But CNPG's reconciler handles pod recreation automatically
(detect missing pod → create). Kuberic must explicitly orchestrate both
deletion and recreation. CNPG has no explicit health check between
replica upgrades — just pod readiness. Kuberic should be more
conservative: verify replication catch-up before proceeding (following
SF's safety-first model).

---

## PG-Specific Considerations

For the PostgreSQL example, upgrades have additional concerns:

### Config-Only Changes

Some PG config changes can be applied via `pg_reload_conf()` without
restart. The operator could detect which parameters changed and:
- **Reload-safe params** (e.g., `max_connections`, `work_mem`):
  Execute `ALTER SYSTEM SET ... ; SELECT pg_reload_conf();`
- **Restart-required params** (e.g., `shared_buffers`, `wal_level`):
  Full pod restart needed

CNPG categorizes parameters as "hot standby sensitive" — those that
require standby restart when increased on primary. The instance manager
handles this automatically.

### PG Major Version Upgrade

Not in initial scope, but the approach would mirror CNPG:
1. Stop PG on primary
2. Run `pg_upgrade --link` (hard links, fast) in-place
3. Start PG with new binaries
4. Rebuild replicas via `pg_basebackup` (timelines reset)

This requires careful orchestration and is a separate design effort.

### Data Directory Preservation

Upgrades MUST preserve PVCs. Pod deletion + recreation attaches the
same PVC — PG data directory survives. The new pod starts PG against
the existing data directory. For replicas, streaming replication
reconnects automatically.

---

## Implementation Plan

### Phase 1: Spec Drift Detection

- Add `spec.image` and `spec.config` fields to CRD
- Compute spec hash on reconcile
- Annotate pods with spec hash at creation time
- Detect drift in reconcile loop (log only, no action)

### Phase 2: Rolling Replica Upgrade

- Implement replica upgrade loop (remove → delete → create → add)
- Add `status.upgrade` progress tracking
- Pause on failure with clear error message
- Integration test: upgrade 3-replica partition image

### Phase 3: Primary Upgrade

- Implement `supervised` / `unsupervised` strategy
- Implement `switchover` method (compose with existing `driver.switchover`)
- Implement `restart` method (direct pod restart)
- Integration test: full rolling upgrade with switchover

### Phase 4: Refinements

- Config-only reload (skip pod restart for reload-safe params)
- Rate limiting / maxUnavailable
- Upgrade history in CRD status
- User-triggered restart annotation

---

## Open Questions

### ~~OQ-1: Pod Identity Across Upgrade~~ (Resolved)

When a replica is removed and re-added, does it get the same replica ID?
The driver's `restart_secondary(id, new_handle)` preserves the ID — it
removes the old entry and adds the new handle with the same ID. This
means `synchronous_standby_names` (application_name = kuberic_{id})
stays consistent. **Resolved by using `restart_secondary`.**

### OQ-2: Mixed-Version Clusters (Deferred)

During a rolling upgrade, the cluster has pods running different versions.
Proto3 wire compatibility (unknown fields ignored) handles additive
changes, but **semantic compatibility** is the real concern — if the
meaning of `committed_lsn` or quorum calculation changes between
versions, old and new pods could disagree on what's committed.

Deferred — not a concern until kuberic has multiple released versions.
When needed: add a `protocol_version` field to replication messages,
define N/N-1 compatibility guarantees.

### OQ-3: Upgrade During Failover

If failover occurs mid-upgrade, the operator should **restart the upgrade
evaluation from scratch** — not resume. Failover changes which pod is
primary, invalidates the `pending` list, and may leave a half-built
secondary. After failover stabilizes: re-scan all pods for spec drift,
rebuild tracking, re-evaluate roles.

**Detailed scenario analysis**:

**Scenario A — Primary dies while secondary pod is being rebuilt**:
The upgrade loop is awaiting the new secondary pod's readiness (step 3d).
The primary crash is detected by the reconciler's health check (which
MUST run in the `Upgrading` phase, not just `Healthy`). The reconciler
transitions to `FailingOver`. `failover()` promotes the remaining live
secondary. The half-built secondary's pod may or may not exist. After
failover stabilizes → re-enter upgrade evaluation.

**Scenario B — Primary dies during `build_replica` inside
`restart_secondary`**: The `build_replica` call fails (primary down).
`add_replica` returns error. **Critical**: RF-3 (zombie replica bug)
must be fixed first — otherwise the zombie is included in failover
quorum and deadlocks `wait_for_catch_up_quorum`. With RF-3 fixed,
the zombie is cleaned up on error, failover proceeds with the remaining
live secondary.

**Design implication**: Health monitoring must be **cross-cutting** —
extracted from the `Healthy` phase handler and run in ALL phases
(Upgrading, Creating, etc.). This matches SF's approach where safety
checks are evaluated continuously. The `Upgrading` handler should
check primary health before each step, and transition to `FailingOver`
if unhealthy — abandoning the current upgrade iteration.

**After failover completes**: The reconciler returns to `Healthy` (or
a new `UpgradePending` state). The upgrade loop restarts from step 1:
re-scan pods for spec drift. The new primary may already be on the new
image (if it was an already-upgraded secondary). The failed old primary,
if rebuilt as a secondary, gets the new spec. Progress tracking is reset.

### OQ-4: Single-Replica Partitions

CRD allows `replicas: 1` (default `min_replicas: 2` would normally
prevent this, but the user can override). With a single replica:

- **Step 3** (upgrade secondaries) is empty — no secondaries exist
- **Step 5 with switchover** is impossible — no target replica

**Two options**:

1. **Block with error**: If `replicas == 1 && method == switchover`,
   reject the upgrade with a clear message. Force user to either add
   a temporary replica first or use `restart` method.

2. **Restart method for single-replica**: This is a special path:
   ```
   a. Delete primary pod (total outage begins)
   b. Create new pod with updated spec
   c. Pod starts, recovers from PVC (OpenMode::Existing)
   d. Operator creates new GrpcReplicaHandle
   e. Re-establish as Primary (no build_replica — no source)
   f. Partition back online (outage ends)
   ```

   This requires a new driver workflow — something like
   `restart_primary(new_handle)` that:
   - Removes old primary from driver (close best-effort)
   - Opens new handle with `OpenMode::Existing` (NOT `New`)
   - Assigns `Primary` role directly (no build step)
   - Doesn't attempt `build_replica` (no source replica)

   **Duration**: PVC recovery time (PG crash recovery: seconds to
   minutes depending on WAL). Total outage for the partition.

**Recommendation**: Option 1 for v1 (block switchover with error, fall
back to restart). Implement the `restart_primary` workflow as part of
the restart primary method (RF-7). Single-replica upgrade is inherently
a downtime event — document this clearly.

### OQ-5: 2-Replica Partitions

With `write_quorum=2` (computed as `total_count/2+1 = 2/2+1 = 2`), the
partition requires both primary and secondary to ACK every write.

**The write stall is confirmed**. During `restart_secondary` →
`add_replica` → `build_replica`, the replicator's quorum tracker still
holds the old config with `write_quorum=2`. The new secondary is
`IdleSecondary` (not in quorum config). Only the primary can ACK →
quorum of 2 is never reached → **all writes hang** until `build_replica`
completes and `reconfigure_quorum` adds the new secondary.

For kvstore (in-memory, milliseconds build) this is negligible. For
PG/SQLite with large state, the stall could be seconds to hours.

**Fix options**:

| Option | Approach | Tradeoff |
|--------|----------|----------|
| A. Pre-reconfigure | Before `build_replica`, reconfigure quorum to `write_quorum=1` (primary-only). After build + promote, reconfigure back to 2. | Reduced durability during build — a primary crash loses uncommitted data. Writes continue. |
| B. Temporary scale-up | Add a 3rd replica first (scale to 3), upgrade the original secondary, remove the temporary replica. | No durability loss, but requires extra resources and is slower (two full builds). |
| C. Block with error | Reject upgrades for partitions where `replicas <= write_quorum`. Require user to scale up first. | Simple, safe, but bad UX for 2-replica users. |
| D. Document and accept | Document that 2-replica upgrades cause a write stall during rebuild. Let users decide. | Honest, but surprising for users expecting "rolling" behavior. |

**Recommendation**: Option A for v1 (retain Option B as alternative for
users who need zero-data-loss upgrades).

**How CNPG handles this**: CNPG dynamically adjusts
`synchronous_standby_names` based on healthy replicas in each reconcile
cycle (`pkg/postgres/replication/legacy.go`). When a replica goes down
during upgrade:
1. `readyReplicas = healthy_pods - 1` → decreases
2. If `readyReplicas < minSyncReplicas` → **self-healing**: lower
   `syncReplicas` to `readyReplicas` (even to 0)
3. `synchronous_standby_names` rewritten with lower count
4. PG reloads → writes continue with fewer (or zero) sync replicas
5. CNPG logs: "Ignore minSyncReplicas to enforce self-healing"

This is exactly Option A — CNPG prioritizes availability over durability
during transient replica loss. Kuberic should do the same: before
removing a secondary for upgrade, reconfigure quorum to the remaining
healthy replica count.

**Note**: This is also relevant for 3-replica partitions if 2
secondaries are being upgraded simultaneously (not possible with
`maxUnavailable: 1`, but worth guarding against).

---

## Review Findings

Findings from 4-specialist Society of Thought review (architecture,
correctness, assumptions, edge-cases). Tracked here for implementation.

### Must-Fix

#### RF-1: `restart_secondary` forces full rebuild — wrong primitive

**Sources**: architecture, assumptions

`restart_secondary` → `add_replica` always uses `OpenMode::New` + full
`build_replica`. For PG/SQLite with PVC-preserved data, every image
upgrade triggers a complete data copy (hours for large databases). The
design claims PVCs preserve data, but the chosen primitive discards it.

**Analysis after investigation**:

All three examples are **file-backed with persistence**:

- **kvstore**: File-backed WAL + snapshot in `data_dir`. `KvState::open()`
  loads snapshot + replays WAL. A pod restart with PVC-preserved data
  could skip `build_replica` and reattach. `OpenMode::Existing` is
  supported at the state layer but never used by the driver.
- **sqlite**: File-backed DB + frames.log in `data_dir`.
  `SqliteState::open()` loads existing DB + replays frames. Same as
  kvstore — persistence works, but driver always forces full copy.
- **PG example**: Doesn't use WalReplicator. `build_replica` runs
  `pg_basebackup` via `CloneFrom` RPC. If PVC preserves PGDATA,
  the secondary can reconnect via streaming replication (same as
  `ReconfigureStandby`). Full `pg_basebackup` is wasteful.

**This is a bug**: All three examples support `OpenMode::Existing` at
the application layer, but the driver's `add_replica` always uses
`OpenMode::New` + full `build_replica`. Pod restarts with PVC-preserved
data unnecessarily copy the entire dataset.

**Required for PG**: A `reconnect_secondary(id, new_handle)` primitive
that: (1) closes old handle, (2) removes from driver, (3) opens new
handle with `OpenMode::Existing`, (4) assigns `ActiveSecondary` role,
(5) reconfigures quorum with `must_catch_up: true`, (6) skips
`build_replica` entirely. The PG secondary reconnects to the primary
via streaming replication and catches up via WAL replay.

**Required for WalReplicator apps**: Either (a) keep using full rebuild
(acceptable if state is small), or (b) add incremental `GetCopyState`
that detects existing state and returns only the delta — a more complex
change to the copy protocol.

**Recommendation**: Implement `reconnect_secondary` for PG upgrades
(v1 — major perf win). Defer incremental copy for WalReplicator apps
to v2 (full rebuild is acceptable for small state).

#### RF-2: PVCs created but never mounted

**Sources**: architecture, assumptions

`build_pod()` (reconciler.rs) has no `volumes` or `volume_mounts`. PVCs
are created but never referenced in pod specs. Data preservation is
non-functional. **Prerequisite** for upgrade implementation — not part
of upgrade design itself, but must be fixed first.

#### RF-3: `add_replica` zombie on failure deadlocks failover

**Sources**: correctness

`add_replica` inserts into `self.replicas` at driver.rs:721 *before*
fallible ops (`build_replica`). If build fails (e.g., primary crashes),
zombie replica stays in the map. **Existing code bug.**

**Verified analysis**: The zombie stays in `self.replicas` with
`Role::None` or `Role::IdleSecondary`. On subsequent `failover()`:
- Zombie is counted in `total_count` → inflated `write_quorum`
- Zombie is listed as `ActiveSecondary` in quorum config (line 389
  hardcodes role) despite actually being `None`/`Idle`
- Zombie's gRPC endpoint may be dead → `send_to_all` wastes bandwidth
- In most cases (3+ healthy replicas), quorum still met and no
  deadlock. Deadlock occurs only if zombie inflation pushes
  `write_quorum` above the number of actually-healthy replicas.

**Severity**: should-fix (code bug, but not a guaranteed deadlock in
typical scenarios — overstated in original review).

**Fix**: Scopeguard pattern — remove from `self.replicas` on error. Or
defer insertion until after `build_replica` succeeds.

#### RF-4: Operator driver state is in-memory only

**Sources**: assumptions

`PartitionDriver` lives in heap memory (`Mutex<HashMap>`). Operator
crash mid-upgrade = lost state, no recovery. CRD status fields are
necessary but insufficient — the driver can't reconstruct handles,
epoch, or quorum config from status alone.

**This is a pre-existing gap** — not specific to upgrades. If the
operator restarts while any partition is `Healthy`, it loses all driver
state. Failover, switchover, and scale operations silently fail
(reconciler logs `"no driver state"` and requeues forever). The only
recovery today is to delete and recreate the partition.

**Fix**: Implement driver reconstruction from running pods' gRPC status
queries (probe all pods → discover roles/epochs → rebuild driver). This
matches CNPG's approach (stateless reconciler, reconstructs each cycle).
Should be implemented as a standalone fix before upgrade work.

### Should-Fix

#### RF-5: Pod deletion before driver notification — stale handle window

**Sources**: architecture, correctness, edge-cases (5 specialists flagged independently)

Design deletes pod (step 3b) before calling `restart_secondary` (3e).
Between these: driver holds stale handle, quorum config includes dead
secondary, failover could select dead pod by cached LSN.

**Fix**: Config-first approach: `remove_secondary` → delete pod → create
pod → `add_replica`. Or call `remove_replica_from_driver(id)` immediately
after deletion.

#### RF-6: 2-replica write stall

**Sources**: edge-cases

With `write_quorum=2`, removing one secondary for upgrade leaves zero
quorum margin. `restart_secondary` doesn't reconfigure quorum before
`build_replica` → writes hang. See OQ-5.

#### RF-7: "restart" primary method under-specified

**Sources**: correctness

Step 5b hand-waves "delete pod → recreate → wait for recovery." Missing:
who triggers failover, epoch handling, PVC data divergence
(`on_data_loss`), downtime bounds. Either fully specify or defer to v2
(switchover only for v1).

#### RF-8: Switchover missing double-catchup

**Sources**: correctness

The design claims SF-style double catchup, but `switchover()` in
driver.rs has no catch-up between `revoke_write_status` and promotion.
**Existing kuberic-core bug** — confirmed by code trace.

**Verified data loss scenario** (3-replica, P=1, S=2=target, S=3):
1. Write W: P=1 self-ACKs + S=3 ACKs → quorum met → client gets OK
2. W in S=2's unbounded channel, drain task hasn't sent yet
3. `revoke_write_status()` — no new writes
4. `change_role(ActiveSecondary)` on P=1 → `close_all()` drops unbounded
   senders → W lost from S=2's channel
5. `change_role(Primary)` on S=2 → S=2 promoted WITHOUT W
6. S=3 has W but is now a secondary — W on S=3, not on primary S=2

W was committed (quorum met) but is missing from the new primary.
Window is microseconds to low milliseconds — `send_to_all` fires
immediately and drain tasks typically deliver before the sequential
`change_role` RPCs complete. But under high write throughput, the
window widens.

**SF's approach** (from `stateful_traits.rs` Rust trait docs):
SF does a catchup BEFORE write revocation (`PreWriteStatusRevokeCatchup`)
with writes still flowing, using `must_catchup` on the target. This
ensures the target is already caught up before writes stop. Then revoke,
then a second catchup for the final drain.

**Recommended fix for kuberic**: Insert a catchup step between
`revoke_write_status` and `change_role(ActiveSecondary)`:

```rust
// 1. Revoke write status (atomic — no new writes)
revoke_write_status().await?;

// 2. NEW: Wait for target to catch up to old primary's progress
//    After revoke, no new writes → LSN is frozen. Poll until target
//    has received all data from the unbounded channel drain.
let target_lsn = self.replicas[&old_primary_id]
    .handle.current_progress();
let deadline = Instant::now() + Duration::from_secs(5);
loop {
    if Instant::now() > deadline {
        // Timeout — target can't catch up (crashed, disconnected).
        // Abort switchover: re-grant write status on old primary.
        // Matches SF's AbortPhase0Demote pattern.
        self.replicas[&old_primary_id].handle
            .state().set_write_status(AccessStatus::Granted);
        return Err(KubericError::Internal(
            "switchover catchup timeout — target unreachable".into()));
    }
    let target_progress = self.replicas[&target_id]
        .handle.current_progress();
    if target_progress >= target_lsn { break; }
    tokio::time::sleep(Duration::from_millis(10)).await;
}

// 3. Demote old primary (now safe — target has all data)
change_role(ActiveSecondary).await?;

// 4. Promote target
change_role(Primary).await?;
```

**Timeout handling**: If the target can't catch up within 5 seconds
(crashed, gRPC disconnected, stuck), the switchover aborts and
write status is re-granted on the old primary. This matches SF's
`AbortPhase0Demote` + `RevertConfiguration` pattern — the partition
continues with the old primary rather than risking data loss.

Since kuberic's revoke is atomic (no writes after step 1), the target's
LSN is frozen — the poll converges quickly (drain task delivers
remaining items from unbounded channel). One catchup is sufficient
(SF's second catchup is redundant when revoke is atomic).

#### RF-9: Single-replica switchover has no target

**Sources**: edge-cases. See OQ-4.

### Consider

- **C-1**: Double upgrade (spec change mid-upgrade) — lock target spec
  version or adopt stateless reconcile approach
- **C-2**: Scale change during upgrade — block or make loop dynamic
- **C-3**: Phase enum needs upgrade variants — CRD schema migration
- **C-4**: Pod scheduling timeout — add `podReadyTimeout`
- **C-5**: Unbounded build duration — add `buildTimeout` + progress
- **C-6**: Spec hash details — document included fields, handle missing
  annotation as "needs upgrade"
- **C-7**: gRPC handle re-creation — document explicit step in sequence
