# Kuberic Operator: Failure Scenarios and Recovery

How the operator reconciler detects and handles various failure modes.
Design informed by CNPG patterns (see `docs/background/cloudnative-pg-architecture.md`).

---

## Pod Lifecycle Signals

| Signal | K8s Source | Detection |
|--------|-----------|-----------|
| Pod missing | Not in `list_pods()` | `driver.replica_ids().len() > pods.len()` |
| Pod not ready | `pod.status.conditions[Ready] == False` | `is_pod_ready(pod) == false` |
| CrashLoopBackOff | `container_status.waiting.reason == "CrashLoopBackOff"` | Specific reason check |
| Pod failed | `pod.status.phase == "Failed"` | Terminal state, won't recover |
| Pod evicted | `pod.status.phase == "Failed"`, `reason == "Evicted"` | Node pressure |
| Pod IP changed | Pod IP differs from GrpcReplicaHandle's address | Pod was recreated |
| Pod pending | `pod.status.phase == "Pending"` | Scheduling, image pull |
| gRPC unreachable | gRPC calls return Unavailable/DeadlineExceeded | `grpc_failure_count >= threshold` |

---

## Requeue Strategy

**Design decision (from CNPG):** Different situations warrant different
requeue intervals. The reconciler should NOT use a single fixed interval.

| Situation | Requeue Interval | Reason |
|-----------|-----------------|--------|
| Failover/switchover in progress | 1s | Poll for completion |
| Pod creation/deletion | 1s | Wait for informer cache |
| Secondary not ready (under threshold) | 5s | Give time to recover |
| All replicas unreachable | 10s | Network issue recovery |
| Optimistic lock conflict on CRD | Immediate | Retry with fresh version |
| Normal reconciliation | No explicit requeue | Watches trigger next run |

---

## Cluster Status & Conditions

**Design decision (from CNPG):** Persist enough state in CRD status to
reconstruct the operator's in-memory state after restart.

### CRD Status Fields

Current fields:
- `epoch`, `currentPrimary`, `targetPrimary`, `phase`
- `stableSnapshot`: authoritative stable epoch, primary logical ID, complete
  logical/incarnation member identities and roles, and write quorum

`currentPrimary` and per-member status remain compatibility/observational
output. They are not used to reconstruct topology.

**New fields needed:**

| Field | Type | Purpose |
|-------|------|---------|
| `failingSinceTimestamp` | `Option<DateTime>` | When primary started failing (for failover delay) |
| `quorumLossSince` | `Option<DateTime>` | When quorum loss was first detected (for wait duration) |
| `instanceNames` | `Vec<String>` | All instance pod names (for missing-pod detection) |
| `instanceStates` | `HashMap<String, InstanceState>` | Per-instance reported state |
| `grpcFailureCounts` | `HashMap<String, u32>` | Consecutive gRPC failures per replica |
| `conditions` | `Vec<Condition>` | Standard K8s conditions |

### Instance State (per-pod)

```rust
struct InstanceState {
    is_primary: bool,
    current_progress: Lsn,      // last known LSN from ReplicaInfo
    pod_ip: String,             // for stale-IP detection
    last_seen: DateTime,        // last successful gRPC contact
}
```

### K8s Conditions

| Condition | Meaning |
|-----------|---------|
| `Ready` | Cluster is fully operational (primary + all replicas healthy) |
| `QuorumAvailable` | Write quorum is met (primary + enough replicas) |
| `Degraded` | One or more replicas unhealthy but quorum maintained |

---

## Failure Scenarios

### 1. Primary Pod Crash / Not Ready

**Detection:** Healthy phase checks `is_pod_ready(primary_pod) == false`.

**Current behavior:** ✅ Implemented
- Transitions to `Phase::FailingOver`
- Calls `driver.failover(primary_id)`
- Selects secondary with highest LSN as new primary
- Updates labels + CRD status
- Returns to `Phase::Healthy`

**Design addition — failover delay (K8s adaptation):**

SF's FM failovers immediately on heartbeat loss (lease-based federation
detection is fast). On K8s, pod readiness probes can flap during transient
issues. An optional delay prevents unnecessary failovers.

This is a **K8s-specific adaptation**, not an SF pattern. Default should
be 0 (immediate, matching SF behavior). Configurable for environments
with unstable pod readiness.

```
Primary not ready detected:
  │
  ├─ If failoverDelay == 0: immediate failover (SF default)
  │
  ├─ If failoverDelay > 0:
  │    ├─ If failingSinceTimestamp is None:
  │    │    Set failingSinceTimestamp = now
  │    │    Requeue after 1s
  │    │
  │    ├─ If elapsed < failoverDelay:
  │    │    Requeue after 1s (keep checking)
  │    │
  │    ├─ If primary recovers:
  │    │    Clear failingSinceTimestamp
  │    │    Stay in Healthy
  │    │
  │    └─ If elapsed >= failoverDelay:
  │         Clear failingSinceTimestamp
  │         Transition to FailingOver
```

**Quorum loss detection (aligned with SF data loss handling):**

SF's FM distinguishes two cases when a primary fails:

1. **Write quorum available** — enough surviving replicas have the data.
   Normal failover: promote replica with highest LSN. This is our
   existing `driver.failover()`.

2. **Write quorum NOT available** — surviving replicas may not have all
   committed ops. SF distinguishes two sub-cases:
   
   a. **Primary alive, secondaries down** — quorum loss. Primary blocks
      writes (`NoWriteQuorum`). FM waits `QuorumLossWaitDuration` for
      secondaries to recover. If timeout expires, FM drops offline replicas
      and declares data loss.
   
   b. **Primary also dead** — FM selects best surviving replica, increments
      `data_loss_number`, promotes with `on_data_loss()` callback.

Our design follows SF's two-phase approach:

```
Failover with quorum check (SF-aligned):
  │
  ├─ Count available replicas with valid LSN
  │
  ├─ If write quorum available (≥ ⌊N/2⌋+1 replicas):
  │    Normal failover: promote highest-LSN replica
  │    No data loss
  │
  ├─ If primary alive but write quorum lost:
  │    Primary already blocks writes (NoWriteQuorum via PartitionState)
  │    Reconciler waits spec.quorumLossWaitDuration (default 60s)
  │    for secondaries to recover
  │    If timeout: drop offline replicas → data loss path below
  │    Set condition QuorumAvailable=False
  │
  └─ If primary dead AND write quorum NOT available:
       Data loss scenario (SF path):
       ├─ Wait spec.quorumLossWaitDuration for any recovery
       ├─ If timeout: increment epoch.data_loss_number
       ├─ Promote best available replica
       ├─ Call on_data_loss() on new primary
       ├─ Rebuild all other replicas from new primary
       └─ Set condition QuorumAvailable=False (informational)
```

**SF nuance discovered from C++ source:** SF does NOT failover immediately
on quorum loss. It waits `QuorumLossWaitDuration` (configurable per-service)
for secondaries to recover. If they do, quorum is restored with no data
loss. Only after timeout does SF proceed with data loss declaration.

This is a middle ground between CNPG (blocks indefinitely) and our
previous design (failover immediately). We should add
`spec.quorumLossWaitDuration` to the CRD.

**Edge case — gRPC unreachable but pod Ready:** See scenario 5.

---

### 2. Secondary Pod Crash / Not Ready

**Detection:** Healthy phase checks ALL replicas for Ready status.

**Current behavior:** ✅ Implemented for stable secondaries. A ready
replacement incarnation uses durable rebuild. An unreachable old incarnation
uses durable force-removal when retained membership can satisfy quorum.

**Implemented flow:**

CNPG detects secondary failures immediately via pod watches and recreates
pods automatically. We should follow the same pattern, but with a grace
period since our pods hold in-memory replicator state.

```
Healthy phase — secondary health check:
  │
  For each secondary in driver.replica_ids():
    Find matching pod in pods list
    │
    ├─ Pod Ready → OK, reset failure tracking
    │
    ├─ Ready pod with new UID:
    │    Durable rebuild under the same logical replica ID
    │
    └─ Missing/unreachable old UID:
         Validate non-primary target, minReplicas, and retained quorum
         Durable target/previous catch-up configuration
         Wait retained quorum + commit reduced current configuration
         Persist reduced stable snapshot
         Remove exact old primary connection
         Best-effort target demote/close
         Delete old pod with UID precondition
    │
    └─ Pod missing → scenario 3
```

**Impact of not handling:** With persisted-mode ACK (acknowledge-gated
quorum), writes may hang waiting for ACK from a dead secondary. With
3 replicas and quorum=2, one secondary failure doesn't block writes
(primary + 1 secondary = quorum). Two secondaries down → writes hang.

---

### 3. Pod Deleted (Missing from list_pods)

**Detection:** Driver has a handle for a replica ID, but no matching pod
exists in `list_pods()`.

**Current behavior:** ✅ Stable missing secondaries are handled by the same
durable force-removal protocol. Missing primaries still route to failover.

**Implemented flow:**

CNPG detects missing pods by comparing expected instances (from CRD
`instanceNames`) against actual pods. We do the same with `driver.replica_ids()`.

```
Healthy phase — missing pod check:
  │
  Compare stableSnapshot members with current pod logical IDs:
      │
      └─ Stable pod not found:
           ├─ Primary → FailingOver
           └─ Secondary → persist durable Force removal
                target/previous configuration → retained quorum
                → current configuration → reduced stable snapshot
                → exact sender cleanup → UID-fenced pod cleanup
                → later durable scale-up restores desired capacity
```

The operator does not call a monolithic force-remove driver method. It
persists the durable operation first, reconfigures the primary using retained
replicas, and treats target lifecycle calls as conditional after membership
commit.

---

### 4. CrashLoopBackOff

**Detection:** `container_status.state.waiting.reason == "CrashLoopBackOff"`

**Current behavior:** Same as "not ready" — only detected for primary.

**Design (informed by CNPG):**

CNPG excludes crash-looping pods from the healthy pool and election
candidates. They cannot be promoted. We should do the same.

```
CrashLoop handling:
  │
  ├─ Crash-looping pods are treated as "not ready" (scenario 2 flow)
  │
  ├─ Excluded from failover election candidates
  │   (driver.failover already selects by LSN from ReplicaInfo;
  │    a crash-looping pod won't respond to GetStatus → no LSN → excluded)
  │
  ├─ After replacementDelay: delete and recreate
  │
  └─ Retry cap:
       Track recreate_count in CRD status per instance
       If recreate_count >= maxRecreateAttempts (default 5):
         Mark instance as Failed
         Log error, set condition Degraded=True
         Don't attempt further replacement
         (app bug requires human intervention)
```

---

### 5. Network Partition (Pod Reachable by Kubelet, Not by Operator)

**Detection:** Pod shows `Ready=True` but gRPC calls from operator timeout.

**Current behavior:** gRPC timeouts surface as errors in reconciler →
controller requeues with backoff. No specific handling.

**Design (SF fencing + K8s-specific detection):**

SF's defense against network partitions is **epoch fencing** — secondaries
reject operations from stale epochs. This is already implemented. K8s adds
complexity because the kubelet and operator have different network views.

#### Level 1: Operator-Side gRPC Failure Tracking (K8s adaptation)

SF's FM uses lease-based heartbeats. On K8s, we use gRPC failure tracking
as an equivalent signal:

```
Per-replica gRPC failure tracking:
  │
  On gRPC error (Unavailable, DeadlineExceeded):
    grpc_failure_count[replica_id] += 1
  │
  On gRPC success:
    grpc_failure_count[replica_id] = 0
  │
  If grpc_failure_count >= grpcFailureThreshold (default 3):
    Treat replica as unreachable
    ├─ Primary → initiate failover (with failover delay if configured)
    └─ Secondary → initiate replacement (with replacement delay)
```

Persist `grpc_failure_count` in CRD status so it survives operator restart.

#### Level 2: Epoch Fencing (SF core mechanism — ✅ implemented)

Our primary defense. When a network partition occurs:
1. Operator detects primary unreachable → failover
2. All secondaries receive `update_epoch(new_epoch)` → reject old primary's ops
3. Old primary's `replicate()` calls fail with quorum errors → `NoWriteQuorum`
4. User code sees `write_status() == NoWriteQuorum` → stops accepting writes

This is the exact SF model. It works even without K8s-specific self-fencing.

#### Level 3: Primary Self-Fencing via Liveness Probe (K8s defense-in-depth)

**This is a K8s-specific addition, not an SF pattern.** SF relies on epoch
fencing + federation heartbeats. On K8s, there's a gap: if the operator
can't reach the primary to send epoch updates, the old primary doesn't
know it's been replaced. The liveness probe fills this gap.

```
Primary liveness probe:
  │
  ├─ Can reach K8s API server?
  │    YES → pass (OK)
  │
  │    NO → Check peer reachability
  │         ├─ Can reach ANY other instance via gRPC data port?
  │         │    YES → pass (OK) — not fully isolated
  │         │
  │         │    NO → FAIL (HTTP 500)
  │         │         Primary is isolated from BOTH API server and all peers
  │         │         → Kubelet restarts pod
  │         │         → Operator detects NotReady → failover
  │
  Replicas: always pass (no benefit to restarting isolated replica)
```

#### Level 4: Multi-Primary Detection

After partition heals, operator may see two pods reporting as primary.
Epoch comparison resolves this deterministically:

```
Multi-primary detection (Healthy phase):
  │
  GetStatus on all replicas
  If multiple report as primary:
    ├─ The one with the OLDER epoch is stale
    ├─ Call close() on the stale primary
    ├─ Delete and recreate the pod
    └─ add_replica to rejoin as secondary
```

---

### 6. Node Failure / Node Drain

**Detection:** Multiple pods become NotReady simultaneously, or node is
marked `Unschedulable`.

**Current behavior:** Primary failure detected → failover. Secondary
failures ignored.

**Design (from CNPG):**

#### Node Drain Handling

CNPG detects drain taints and proactively moves the primary off draining
nodes. We should add a similar mechanism:

```
Healthy phase — node drain detection:
  │
  For each pod:
    Get pod's node
    If node.spec.unschedulable == true or drain taint present:
      ├─ If pod is primary:
      │    Find secondary on a schedulable node
      │    Trigger switchover to that secondary
      │
      └─ If pod is secondary:
           Mark for replacement after drain completes
           (PDB protects against too-fast eviction)
```

#### Pod Anti-Affinity (Prevention)

Add to CRD spec and pod template generation:

```yaml
spec:
  affinity:
    podAntiAffinityType: preferred  # or "required"
```

The operator generates anti-affinity rules:

```yaml
affinity:
  podAntiAffinity:
    preferredDuringSchedulingIgnoredDuringExecution:  # or required
    - weight: 100
      podAffinityTerm:
        labelSelector:
          matchLabels:
            kuberic.io/set: {set_name}
        topologyKey: kubernetes.io/hostname
```

**Default: `preferred`** — pods spread across nodes when possible but
can colocate if scheduling fails. `required` enforces strict separation
(may prevent scheduling if not enough nodes).

---

### 7. Quorum Loss (Majority of Replicas Down)

**Detection:** Primary's `write_status()` returns `NoWriteQuorum`, or
gRPC calls to majority of replicas fail.

**Current behavior:** ❌ Not detected by operator

**Design (aligned with SF quorum loss / data loss protocol):**

SF distinguishes **quorum loss** (runtime state: writes blocked, system
waits) from **data loss** (explicit FM decision: committed ops may be
irrecoverable). Quorum loss may recover without data loss if replicas
come back before `QuorumLossWaitDuration` expires.

See `docs/background/service-fabric/failover.md` §Quorum Loss
and Data Loss for the full SF protocol.

#### Case 1: Primary alive, quorum lost

```
Primary is up but write_status == NoWriteQuorum:
  │
  ├─ Detection: periodic GetStatus on primary
  │
  ├─ Set condition QuorumAvailable=False
  ├─ Record quorumLossSince = now (in CRD status)
  │
  ├─ Wait for replicas to recover:
  │    Requeue every 5s
  │    Primary already blocks writes (NoWriteQuorum via PartitionState)
  │    Reads still work on primary
  │
  ├─ If replicas recover (quorum restored):
  │    Clear quorumLossSince
  │    Set condition QuorumAvailable=True
  │    Writes resume automatically (no operator action needed)
  │    *** NO DATA LOSS ***
  │
  └─ If quorumLossWaitDuration expires:
       Drop offline replicas from configuration
       → data loss protocol (same as §1 failover with quorum check)
       Increment epoch.data_loss_number
       Call on_data_loss() on primary
       Rebuild secondaries from primary
```

This matches SF's `ReconfigurationTask::CheckFailoverUnit()` behavior:
wait `QuorumLossWaitDuration`, then `DropOfflineReplicas()`.

#### Case 2: Primary dead, quorum lost (data loss scenario)

Covered by the failover protocol (§1). When write quorum is not available
among survivors:
1. Wait `quorumLossWaitDuration` for replicas to recover
2. If timeout: increment `data_loss_number`, promote best available,
   call `on_data_loss()`
3. User's handler decides: accept loss, restore, or error

#### Case 3: Quorum loss during reconfiguration

SF also checks **read quorum** on the *previous* configuration during
reconfiguration. If `PC.UpCount < PC.ReadQuorumSize`, data from the
previous epoch may not be preserved. This is a subtle data loss scenario
that we should handle in `PartitionDriver` when dual-config quorum is
active.

**Key principle (from SF):** The system eventually makes progress, but
waits first. `QuorumLossWaitDuration` is the knob that trades
availability (longer wait = longer write outage) for consistency (more
time for replicas to recover without data loss).

**New CRD field:** `spec.quorumLossWaitDuration` (default 60s).

**Prevention (matching SF):**
- Use `spec.minReplicas` ≥ 2 to require quorum for writes
- Use pod anti-affinity across nodes (fault domain spreading)
- Synchronous replication (our default via persisted-mode ACK)
- Set `quorumLossWaitDuration` per application needs

---

### 8. Operator Crash / Restart

**Detection:** N/A — the operator itself restarts.

**Current behavior:** ✅ Stable `Healthy`, durable `Switchover`, and durable
replica add/rebuild recovery are implemented. The CRD snapshot survives in etcd
while current pod metadata and runtime status attest that it still describes
the live partition.

```
Operator restart — first reconcile per KubericSet:
  │
  ├─ Require status.stableSnapshot:
  │    stable epoch, primary ID, complete members/roles, write quorum
  │
  ├─ List pods with label kuberic.io/set={name}:
  │    For each pod:
  │      Derive logical ID from the required pod-index label
  │      Create GrpcReplicaHandle from current pod UID and addresses
  │
  ├─ PartitionDriver::recover(snapshot, handles):
  │    Call only GetStatus
  │    Validate logical/incarnation bijection, exact epoch and stable roles
  │    Validate one primary, complete membership, and majority write quorum
  │    Rebuild handles and current configuration without runtime mutation
  │
  ├─ Validate consistency:
  │    Any absent/duplicate/relabelled/reincarnated member → fail closed
  │    Any runtime epoch/role/incarnation mismatch → fail closed
  │    Runtime unhealthy but otherwise consistent → recover, then health logic
  │
  └─ Resume normal reconciliation
```

`currentPrimary` is refreshed from recovered driver state and is not trusted
as input. Legacy status without `stableSnapshot` is rejected. Stable topology
changes persist a fresh snapshot; multi-member add/remove loops patch after
each committed change. If runtime mutation succeeds but status persistence
does not, the live operator retries that exact pending status before another
action. If its process state is also lost, a subsequent recovery sees the old
snapshot and fails closed. Creating probes runtime status and refuses to replay
creation when replicas are already initialized.

During `Switchover` and `AddingReplica`, `status.operation` is the authoritative
compact checkpoint. It stores previous/target snapshots, exact target
incarnations, the current phase, and one write-ahead correlated action. Each
reconcile reconstructs fresh handles, observes
role/epoch/incarnation/progress/write/configuration/activity state, and either
advances one checkpoint, dispatches one activity, waits, compensates, or
poisons. Add/rebuild observes long-running copy progress and restores the
previous current configuration before candidate cleanup when catch-up had
started. Kubernetes `resourceVersion` rejects stale concurrent advancement.

**Boundary:** This does not recover `Creating`, `FailingOver`, scale-down/
remove, other mid-configuration paths, or continue an operation after its
target incarnation changes. Those cases require additional durable-operation
migrations or a separate pod-state recovery protocol.

---

### 9. Stale/Zombie Primary (Split Brain)

**Detection:** After failover, the old primary pod may still be running
and accepting writes if it didn't receive the epoch fence.

**Defense layers (SF core + K8s additions):**

| Layer | Mechanism | Origin | Status |
|-------|-----------|--------|--------|
| 1. Epoch fencing | `update_epoch()` on secondaries before promote. Secondaries reject old-epoch ops. | **SF core** | ✅ Implemented |
| 2. Role status | `set_status_for_role()` sets `NotPrimary` on demotion. `StateReplicatorHandle::replicate()` rejects. | **SF core** | ✅ Implemented |
| 3. Primary self-fencing | Liveness probe isolation check (see scenario 5, level 3). Pod self-kills if isolated. | **K8s addition** (from CNPG) | ❌ Not implemented |
| 4. Old primary cleanup | Operator verifies old primary pod is closed/deleted after failover. | **K8s addition** | ❌ Not implemented |
| 5. Multi-primary detection | Healthy phase detects two primaries via GetStatus. Closes stale one (epoch comparison). | **K8s addition** | ❌ Not implemented |

**SF's primary defense is epoch fencing (layers 1-2).** Layers 3-5 are
K8s-specific additions that cover gaps where the operator can't reach pods
to deliver epoch updates. SF doesn't need these because the federation
subsystem provides reliable failure detection via lease-based heartbeats.

**Layer 4 — old primary cleanup:**

```
After failover completes:
  │
  ├─ If old primary pod still exists:
  │    Try close() on old primary handle
  │    If gRPC fails (unreachable):
  │      Delete the pod (force kill)
  │    Create replacement pod
  │    add_replica as secondary
  │
  └─ If old primary pod missing:
       Already gone, create replacement if needed
```

---

## Implementation Priority

| Priority | Scenario | Effort | Origin |
|----------|----------|--------|--------|
| P0 | Secondary not ready → replace | Medium | SF (FM detects + replaces) |
| P0 | Pod deleted → detect + replace | Medium | SF (FM detects) + K8s pod mgmt |
| P0 | Data loss failover (quorum lost) | Medium | **SF** (`on_data_loss()` protocol) |
| Done | Stable Healthy operator restart → recover driver | Implemented | SF (FM is stateless, uses persistent config) |
| P1 | gRPC failure tracking | Medium | K8s adaptation (replaces SF federation heartbeats) |
| P1 | Old primary cleanup after failover | Small | K8s addition |
| P1 | CRD conditions (Ready, Degraded, Quorum) | Small | K8s addition (from CNPG) |
| P1 | Failover delay (optional) | Small | K8s adaptation (from CNPG) |
| P2 | Primary self-fencing liveness probe | Medium | K8s addition (from CNPG isolation check) |
| P2 | Node drain detection | Medium | K8s addition (from CNPG, analogous to SF PLB) |
| P2 | Multi-primary detection | Small | K8s addition (epoch comparison) |
| Done | Durable scale-down + force-remove secondary | Implemented | SF (`RemoveReplica`, `RemoveFromCurrentConfiguration`) |
| P3 | CrashLoop retry capping | Small | K8s addition |
| P3 | Pod anti-affinity in CRD | Small | K8s addition (analogous to SF fault domains) |

---

## Reconciler Phase Diagram (Updated)

```
                    ┌─────────┐
                    │ Pending  │
                    └────┬─────┘
                         │ create pods
                    ┌────▼─────┐
                    │ Creating │ ◄── wait for all pods ready
                    └────┬─────┘
                         │ create_partition via driver
                    ┌────▼─────┐
          ┌────────►│ Healthy  │◄──────────────────────┐
          │         └─┬──┬──┬──┘                        │
          │           │  │  │                           │
          │    primary│  │  │target_primary             │
          │    unhealthy │  │!= current_primary         │
          │    (after │  │  │                           │
          │    delay) │  │  │                           │
          │    ┌──────▼┐ │ ┌▼──────────┐               │
          │    │Failing │ │ │Switchover │               │
          │    │Over    │ │ │           │               │
          │    └───┬────┘ │ └─────┬─────┘               │
          │        │      │       │                     │
          │   failover    │  switchover                 │
          │   (+ data loss│       │                     │
          │    if quorum  │       │                     │
          │    lost)      │       │                     │
          │        │      │       │                     │
          └────────┘      │       └─────────────────────┘
                          │
                    scale-up: durable add
                    scale-down: durable config-first remove
                    secondary health: durable rebuild or force-remove
                    node drain: switchover primary off draining node
```

**SF-aligned failover:** Unlike CNPG which blocks indefinitely on quorum
loss, we follow SF's data loss protocol. If write quorum is available,
normal failover. If not, promote best available with `on_data_loss()`.
The system always makes progress.

**New in Healthy phase:**
- Secondary health monitoring (replace unhealthy replicas)
- Missing pod detection (replace deleted pods)
- Node drain detection (switchover off draining nodes)
- gRPC failure tracking (detect unreachable but "Ready" pods)
- Multi-primary detection (close stale primary)
