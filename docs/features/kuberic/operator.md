# Kuberic: Operator Design

The Kuberic operator acts as SF's Failover Manager on Kubernetes.
It watches `KubericSet` CRDs and orchestrates pod lifecycle, failover,
switchover, and scaling via `PartitionDriver`.

> Part of the [Kuberic Design](../kuberic-replicator-design.md).
> Failure scenarios documented in [operator-failure-scenarios.md](../operator-failure-scenarios.md).

---

## CRD

**`KubericSet`** with:

**Spec fields:**
- `spec.replicas`, `spec.minReplicas`
- `spec.failoverDelay`, `spec.switchoverDelay`, `spec.replacementDelay`
- `spec.quorumLossWaitDuration`
- `spec.grpcFailureThreshold`, `spec.maxRecreateAttempts`
- `spec.podAntiAffinityType`

**Status fields:**
- `epoch`, `currentPrimary`, `targetPrimary`, `phase`
- `reconfigurationPhase`
- optional authoritative `stableSnapshot` with epoch, primary logical ID,
  complete member logical/incarnation identities and roles, and write quorum
- optional compact versioned `operation` checkpoint for durable creation,
  switchover, replica add/rebuild, and configuration-first replica removal,
  including optional previous/committed bootstrap topology, target snapshot,
  and one pending correlated action
- `failingSinceTimestamp`, `quorumLossSince`
- per-member stable replica ID and replica incarnation (Kubernetes Pod UID)
- `instanceNames`, `instanceStates`
- `conditions`

**Reconciliation:** Stable workflows use `PartitionDriver` with
`GrpcReplicaHandle` (same driver as tests, different transport). A stable
operation is complete only after its resulting snapshot is persisted. Durable
creation persists partial committed bootstrap topology after primary-only and
each expanded current configuration, so process loss rolls forward from live
committed authority instead of replaying `Open(New)`.

Reconciler-driven creation, switchover, replica add/rebuild, and replica
removal persist a versioned operation checkpoint, reconstruct fresh handles
and observations on every reconcile, and advance one transition or activity
at a time. Pending action intent is durable before RPC, pod-label, or
UID-fenced pod-delete mutation. Status patches include the observed Kubernetes
`resourceVersion`; no lock is held across a durable activity.

---

## Reconciler Phases

| Phase | Description |
|-------|-------------|
| `Pending` | CRD created, no pods yet |
| `Creating` | Durable initial bootstrap, partial topology commit, and routing publication |
| `Healthy` | Normal operation — monitors health, handles scale, detects failures |
| `FailingOver` | Primary failed, running failover protocol (incl. data loss path) |
| `Switchover` | Planned primary change in progress |
| `AddingReplica` | Durable scale-up or stale-secondary replacement in progress |
| `RemovingReplica` | Durable healthy scale-down or stale/dead-secondary eviction in progress |

Note: No `WaitingForQuorum` phase. Following SF's design, the system always
makes progress during failover. If quorum is lost, the data loss protocol
kicks in (`on_data_loss()`) rather than blocking.

---

## Healthy Phase Responsibilities

The Healthy phase runs multiple checks on each reconciliation:

```
Healthy phase:
  │
  ├─ 1. Primary health check
  │    Pod ready? gRPC reachable?
  │    → If not: enforce failover delay → FailingOver
  │
  ├─ 2. Switchover detection
  │    targetPrimary != currentPrimary?
  │    → If yes: → Switchover
  │
  ├─ 3. Secondary health check
  │    Ready replacement incarnation? → durable rebuild
  │    Old incarnation unreachable with retained quorum? → durable force-remove
  │
  ├─ 4. Missing pod detection
  │    driver.replica_ids() vs list_pods()
  │    → If replacement is ready: rebuild; otherwise durable force-remove
  │
  ├─ 5. Scale reconciliation
  │    spec.replicas vs current count
  │    → Scale up: create pods + add_replica
  │    → Scale down: durable configuration-first removal
  │
  ├─ 6. Node drain detection
  │    Any pod's node unschedulable?
  │    → If primary: switchover to healthy node
  │
  ├─ 7. Multi-primary detection
  │    GetStatus on all replicas
  │    → If multiple primaries: close stale one
  │
  └─ 8. Condition updates
       Set Ready/Degraded/QuorumAvailable conditions
```

---

## gRPC Failure Tracking

Per-replica `grpc_failure_count` persisted in CRD status. Incremented
on Unavailable/DeadlineExceeded, reset on success. When count reaches
`spec.grpcFailureThreshold` (default 3), treat replica as unreachable
even if pod shows Ready.

### Replica Incarnations

The stable replica ID is derived from the pod ordinal and survives pod
recreation. Each concrete pod uses its Kubernetes UID as
`ReplicaInstanceId`. Runtime status, driver handles, replica-set
configurations, and CRD member status carry that incarnation.

Health reconciliation compares the runtime-reported incarnation with the
handle's expected pod UID. Before a stale secondary is removed from driver
state, the driver sends a precise `(replica ID, incarnation)` removal to the
primary. Re-adding the ordinal installs the new endpoint and cancels the old
connection's drain and acknowledgement tasks.

---

## Operator Restart Recovery

On the first `Healthy` reconcile after process restart, the operator requires
`status.stableSnapshot`, derives logical IDs from required pod-index labels,
and creates handles from current pod UIDs and addresses. `PartitionDriver::
recover()` calls only `GetStatus`, then requires an exact logical/incarnation
bijection, epoch and stable-role agreement, one primary, complete membership,
and the persisted majority write quorum.

The snapshot is authoritative. `currentPrimary` remains compatibility output
and is refreshed from recovered driver state; it is never recovery input.
Legacy resources without a snapshot, changed pod identities, and live state
that is newer or otherwise inconsistent fail closed. Runtime health does not
invalidate an otherwise consistent snapshot: recovery completes first, then
the normal health/failover path runs. Stable recovery is intentionally limited to `Healthy` topology. Durable
`Creating`, `Switchover`, `AddingReplica`, and `RemovingReplica` resume from
`status.operation`; failover/data-loss migration remains unsupported. See
`operator-failure-scenarios.md` §8.

## Durable Partition Creation

All initial pods are created with `kuberic.io/role=bootstrap`, which does not
match the primary or secondary client Services. The durable operation first
fences any pre-existing initial serving labels back to `bootstrap`. After every
desired pod is ready, the operator validates pod-index-derived logical IDs and
UIDs, sorts by logical ID, selects the lowest ID as initial primary, and
persists a `CreatePartition` operation before runtime mutation.

The operation explicitly has no previous stable topology. It records the full
target and an optional committed bootstrap snapshot. Primary-only current
configuration is committed and checkpointed first. Each secondary then runs
Open(New), epoch update, idle role, build, active role, catch-up configuration,
write-quorum wait, and current configuration; the expanded partial topology is
persisted before the next secondary starts.

Controller replacement observes runtime postconditions and correlated
activity state. Committed members are never reopened or rebuilt. Failure
before the first commit cleans up and restarts from no topology; later failures
preserve committed members and clean up only the current candidate. Final
routing labels are published one member per reconcile only after the complete
target (and therefore `minReplicas`) is durably committed.

The fencing iterator addresses the exact member at its persisted index, not
the current runtime candidate field. Fence failure never schedules
RemoveReplica, demotion, close, or deletion. Same-incarnation unavailability
fails closed; an uncommitted replacement restarts with a new persisted target
UID while retaining committed members. All compensation phases reject a target
already present in the committed bootstrap snapshot.

## Durable Switchover

The internal durable layer separates pure observation/decision logic from
side-effecting activities. Its compact checkpoint pins operation version,
source and target stable snapshots, phase, frozen LSN, retry/deadline/error
metadata, and at most one pending action. Each action identifies the exact
replica ID, pod-UID incarnation, expected epoch, and desired postcondition.

`GetStatus` exposes write access, canonical configuration state, the last
completed durable action, and the scheduled/in-progress/completed/failed state
of the current durable activity. Lost replies therefore resume by observation
rather than blind RPC repetition. Target-promotion failure can durably restore
the old primary; impossible or stale observations poison the operation without
publishing a new stable snapshot.

---

## Scale-Up Reconciliation

When `spec.replicas > current stable member count`:
1. Create new Pod (with ownership labels)
2. Wait for Pod Ready
3. Persist a versioned add operation containing the previous stable snapshot
   and the exact candidate pod UID
4. Advance one correlated activity per reconcile:
   Open(New) → UpdateEpoch → IdleSecondary → BuildReplica on the primary →
   ActiveSecondary
5. Install catch-up configuration with the candidate marked `must_catch_up`,
   wait for write quorum, then install current configuration
6. Update the candidate label and publish the target stable snapshot

`BuildReplica` is scheduled asynchronously by the pod runtime. Status reports
scheduled, in-progress, completed, or failed, so retries and controller
replacement do not start a concurrent second copy.

Stale-secondary replacement uses the same operation after first removing the
old exact `(ReplicaId, ReplicaInstanceId)` connection from the primary. The
previous stable snapshot remains unchanged until the replacement's current
configuration commits.

---

## Scale-Down Reconciliation

When `spec.replicas < current pod count`:

Design follows SF's config-first approach (remove from quorum before closing):

```
1. Operator selects secondary to remove (prefer newest, never primary)
2. driver.remove_secondary(replica_id):
   a. Verify replica_count > min_replicas (safety)
   b. update_catch_up_configuration (new config WITHOUT the replica)
   c. wait_for_catch_up_quorum(Write)
   d. update_current_configuration (finalize)
   e. change_role(None) on removed replica
   f. close removed replica
   g. Remove from driver
3. Operator deletes Pod + PVC
4. Update CRD status
```

**Safety:**
- Cannot scale below `spec.minReplicas`
- Config update happens BEFORE close — write quorum is maintained
  because the old config still includes the replica until finalized
- If primary is targeted, operator must switchover first, then remove
  the demoted secondary

**Selection heuristic:** Prefer the secondary with the highest replica ID
(newest). CNPG uses the same approach. SF uses PLB load balancing which
is more sophisticated but unnecessary for our initial implementation.

**All gRPC calls have timeouts.** Ordinary control calls use the default 30s
bound; durable BuildReplica allows a 10-minute copy window while remaining
observable through GetStatus. One reconfiguration runs at a time.
