# Kubelicate: Operator Design

The Kubelicate operator acts as SF's Failover Manager on Kubernetes.
It watches `KubelicateSet` CRDs and orchestrates pod lifecycle, failover,
switchover, and scaling via `PartitionDriver`.

> Part of the [Kubelicate Design](../kubelicate-replicator-design.md).
> Failure scenarios documented in [operator-failure-scenarios.md](../operator-failure-scenarios.md).

---

## CRD

**`KubelicateSet`** with:

**Spec fields:**
- `spec.replicas`, `spec.minReplicas`
- `spec.failoverDelay`, `spec.switchoverDelay`, `spec.replacementDelay`
- `spec.quorumLossWaitDuration`
- `spec.grpcFailureThreshold`, `spec.maxRecreateAttempts`
- `spec.podAntiAffinityType`

**Status fields:**
- `epoch`, `currentPrimary`, `targetPrimary`, `phase`
- `reconfigurationPhase`, `currentConfiguration`, `previousConfiguration`
- `failingSinceTimestamp`, `quorumLossSince`
- `instanceNames`, `instanceStates`
- `conditions`

**Reconciliation:** Uses `PartitionDriver` with `GrpcReplicaHandle`
(same driver as tests, different transport). CRD status is the durable
state machine — write-ahead pattern (CRD write before gRPC calls).

---

## Reconciler Phases

| Phase | Description |
|-------|-------------|
| `Pending` | CRD created, no pods yet |
| `Creating` | Pods created, waiting for ready + partition initialization |
| `Healthy` | Normal operation — monitors health, handles scale, detects failures |
| `FailingOver` | Primary failed, running failover protocol (incl. data loss path) |
| `Switchover` | Planned primary change in progress |

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
  │    All secondaries ready? gRPC reachable?
  │    → If not (after replacementDelay): replace
  │
  ├─ 4. Missing pod detection
  │    driver.replica_ids() vs list_pods()
  │    → If mismatch: force_remove + replace
  │
  ├─ 5. Scale reconciliation
  │    spec.replicas vs current count
  │    → Scale up: create pods + add_replica
  │    → Scale down: remove_secondary + delete pod
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

---

## Operator Restart Recovery

On first reconcile after restart, reconstruct `PartitionDriver` from
CRD status + pod list via `PartitionDriver::recover()`. See
`operator-failure-scenarios.md` §8 for full design.

---

## Scale-Up Reconciliation

When `spec.replicas > current pod count`:
1. Create new Pod (with ownership labels)
2. Wait for Pod Ready
3. Connect `GrpcReplicaHandle` to new pod
4. `driver.add_replica(handle)` — builds via copy stream, joins quorum
5. Update CRD status with new configuration

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

**All gRPC calls have timeouts** (default 30s). One reconfiguration at a
time. Phases are idempotent (safe to re-execute after operator crash).
