# ADR: Pod-Local RA-Lite Control Boundary

> **Status:** Implemented
> **Date:** 2026-08-31

Inserted `ReplicaAgent` between the gRPC control server and `PodRuntime`.
The agent owns pod-local manager-protocol acceptance and observation;
the runtime owns ordered service/replicator effects.

---

## Problem

`PodRuntime` previously held the current correlated action, validated action
IDs/signatures, classified duplicate/conflicting work, spawned long-running
actions, retained their result, and also executed service/replicator effects.
A container restart under the same Kubernetes Pod UID therefore had no
separate identity for the new process, and absent volatile correlation state
could be mistaken for evidence that an old action had not run.

## Decision

Use two channel boundaries:

```
ControlServer → ReplicaAgent → PodRuntime
```

`ReplicaAgent` owns:

- replica/process/epoch/control-version fences;
- legacy and versioned correlated admission;
- one active action and 16 terminal observations;
- duplicate replay and retained signature conflicts;
- capacity-16 direct mutation serialization;
- generation-qualified runtime completion tokens; and
- bounded local fault/error status.

`PodRuntime` owns:

- open, role, epoch, configuration, catch-up, build/remove, data-loss, and
  close effects;
- established service/replicator callback order; and
- exact typed completion.

Runtime status is published through `RuntimeControlSnapshot` plus shared
`PartitionState`, keeping agent status readable while an effect runs.

## Process Identity

Pod UID remains `ReplicaInstanceId`. `AgentGeneration` is a separate random
value created on every process start. `AgentControlVersion` advances on
distinct mutation admission only.

Agent observations never cross generations. A prior-generation request is
stale even if the Pod UID is unchanged. CRD status remains the only durable
workflow store; it records optional dispatch fences, not the local action
buffer.

## Protocol Compatibility

`ExecuteCorrelatedControlAction` is additive and strictly validates every
safety field. Existing RPCs and `ExecuteDurableAction` remain available. The
legacy correlated method uses the same agent ledger without the new fences.

The operator persists an explicit `correlatedControlV1` or `legacy` selection
before activity. Capability absence can select legacy; a rejected versioned
request never silently falls back.

## Consequences

- Duplicate reply replay is exact while the terminal is retained in the same
  reachable generation.
- Evicted/old-version continuity fails closed without an exactly-once claim.
- Same-Pod process restart is observable independently of Pod replacement.
- Unverifiable stable secondary process state enters the existing durable
  force-remove/rebuild path.
- Direct close tears down the endpoint, so its terminal replay is best effort.
- Fault publication is bounded and best effort, not a global health history.

## Service Fabric Mapping

The ownership split follows SF RA versus RAProxy:

- RA accepts/fences FM messages and serializes per failover unit.
- RAProxy binds runtime identity and executes ordered service/replicator
  action lists.

Kuberic deliberately does not copy RA's durable local failover-unit store.
Kubernetes CRD status and the operator remain the global durable authority.
