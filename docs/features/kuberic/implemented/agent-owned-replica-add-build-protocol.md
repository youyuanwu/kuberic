# ADR: Agent-Owned Replica Add/Build Protocol

> **Status:** Implemented
> **Date:** 2026-09-01

## Problem

Replica add/rebuild was durable in CRD status, but the operator still sent each
target lifecycle, copy, catch-up, quorum, and configuration action separately.
This kept primary-RA and target-RA sequencing in the FM analogue and made agent
restart recovery depend too heavily on one bounded action record.

Service Fabric sends AddReplica from FM to the current primary RA
(`/data/code/service-fabric/src/prod/src/Reliability/Failover/fm/SendMessageAction.cpp:174-180`).
The primary RA owns duplicate/stale add processing and sends CreateReplica to
the target RA
(`/data/code/service-fabric/src/prod/src/Reliability/Failover/ra/ReconfigurationAgent.cpp:1848-2042,3452-3467`).
The target validates identity/epoch, opens locally, and replies
(`/data/code/service-fabric/src/prod/src/Reliability/Failover/ra/ReconfigurationAgent.cpp:2244-2414,840-928`).
The primary then invokes its local replicator build and ignores stale replies
(`/data/code/service-fabric/src/prod/src/Reliability/Failover/ra/ReconfigurationAgent.cpp:2851-2941,4027-4089`).

## Decision

Use this production flow:

```
operator
  → one versioned AddReplicaIntent
  → current primary ReplicaAgent coordinator
      → target ReplicaAddBuildPeer Prepare
      → primary PodRuntime tracked copy
      → target ReplicaAddBuildPeer Activate
      → primary catch-up / quorum / current configuration
  → operator commit recognition, label, stable publication
```

The operator retains topology, placement, pod lifecycle, epoch authority,
durable intent, `resourceVersion` fencing, irreversible commit recognition,
and stable publication. Agents retain only bounded transient coordination.
`PodRuntime` retains ordered effects and one current/latest build observation.

## Protocols and Versions

- `ReplicatorControl` still exposes only `GetStatus` and
  `ExecuteCorrelatedControlAction`.
- Required correlated control protocol: **2**.
- `ReplicaAddBuildPeer` shares the control listener and exposes only status and
  one stage-execution RPC.
- Required peer protocol: **1**.
- Add durable operation version: **2**. Non-add durable workflows remain
  version 1.

No compatibility fallback or direct target mutation path exists.
Superseded fine-grained add phases, action kinds, and encoded add payload
status are removed; operators must quiesce add/rebuild before coordinated
deployment.

## Safety Decisions

### Frozen structural configuration

The CRD intent freezes exact membership, identities, endpoints, roles,
`must_catch_up`, non-target progress, quorum, and a target
`BuildCopyLsn` progress source. The target's progress is materialized only from
the completed copy boundary. This avoids changing an accepted action's meaning
while preserving catch-up seeding.

### Target authority

Before every target effect, the target reverse-observes the exact active parent
action and primary incarnation/generation/epoch/role/write status. Cleanup also
checks that current configuration does not contain the target. Exact duplicate
peer messages replay; conflicts and stale authority perform no effect.

### Copy evidence

Tracked build evidence includes target agent generation. A target process
restart under the same Pod UID changes generation and requires a new copy.
Copy LSN is published only after target acknowledgement. The boundary is
non-consuming, exact-incarnation qualified, and add catch-up carries the exact
semantic build key.

### Commit and compensation

Before current-configuration commit, compensation restores previous
configuration when needed, removes the exact target connection, and requests
target cleanup. `Compensated` is emitted only after the primary barrier is
observed; otherwise the operation is poisoned and the target is preserved.

After current-configuration commit, rollback is forbidden. Successful exact
attestation publishes a serving secondary. Bounded failure publishes the
proven membership as `CommittedDegraded`, removes any serving label, and hands
repair to existing Healthy/failover/rebuild behavior.

## Consequences

- Production add/rebuild issues one operator mutation to the primary and none
  to the target.
- Agent status remains available during copy and quorum wait.
- Missing agent history is not treated as non-execution.
- Same-Pod target process restart recopies conservatively.
- Full primary loss after target activation without commit evidence remains
  fail-closed to avoid deleting possible acknowledged state.
- The peer protocol cannot grow into a general reconfiguration framework
  without a new explicit decision.

## Remaining Differences from Service Fabric

SF persists local failover-unit metadata but keeps message stage flags
transient
(`/data/code/service-fabric/src/prod/src/Reliability/Failover/ra/FailoverUnit.h:566-578`,
`/data/code/service-fabric/src/prod/src/Reliability/Failover/ra/Replica.h:202-206,230-257`).
Kuberic keeps all durable global state in Kubernetes CRD status and no durable
agent-local failover-unit store.

SF can reopen a persisted standby during CreateReplica. Kuberic retains its
existing `Open(New)` add/rebuild behavior. SF also has broader RA-to-RA
Deactivate/Activate/GetLSN protocols; Kuberic's peer service is deliberately
limited to add/build.
