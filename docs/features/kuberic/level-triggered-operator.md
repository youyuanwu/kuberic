# Level-Triggered Kuberic Operator

## Status

The `operator.kuberic.io/v1alpha1` stack is the only Kuberic operator path.
It is experimental and intended for development and CI. Images are not
published and no production activation procedure is provided.
This page describes **CURRENT** behavior. The
[controller/local-agent/host redesign](engine-neutral-hosting.md#controller-to-host-messages-and-action-selection)
and [R1–R5 runtime roadmap](sf-v1-semantic-alignment-plan.md#hosting-redesign-roadmap--not-implemented)
are **NOT IMPLEMENTED**; no Kubernetes resource/control behavior changes here.

## Architecture

| Component | Responsibility |
|---|---|
| `kuberic-controller` | Namespace observation, resource convergence, accepted Public V1 status, and fenced command dispatch |
| `kuberic-runtime` | Replica hosting, local agent durability, lifecycle execution, replication/copy transport, and provider integration |
| application provider | Durable application bytes, progress, retained operations, and copy snapshots |

Every reconcile re-observes the `KubericSet`, owned Pods, PVCs, Services,
Secrets, and agent reports. Stable and waiting states requeue; watch delivery
is an optimization.

## Installation Namespace

The controller requires `KUBERIC_INSTALLATION_NAMESPACE`. Watches and mutable
Kubernetes clients are namespaced, and deployment RBAC uses `Role` and
`RoleBinding`.

There is no cluster-wide mutation client and no Kubernetes cutover Lease.

## Resource

```yaml
apiVersion: operator.kuberic.io/v1alpha1
kind: KubericSet
metadata:
  name: kvstore2
  namespace: kuberic-system
spec:
  replicas: 3
  image: localhost/kvstore2:level-triggered-v1
  failoverDelaySeconds: 10
```

`spec.replicas` must be positive. `spec.image` is the application image.
`spec.failoverDelaySeconds` defaults to 30.

Status is structurally typed and contains only:

- `publicV1Authority`;
- `publicV1Topology`;
- `publicV1SessionRenewal`;
- `publicV1Replacement`;
- `publicV1Transition`;
- `publicV1Operation`.

Unknown, legacy, or partial status is rejected before planning or effects.

## Fresh Bootstrap

Before consuming reports, the controller converges:

1. agent credentials Secret;
2. headless peer Service;
3. disabled write Service;
4. one PVC and Pod for each desired replica ID;
5. one exact control/replication endpoint per Pod incarnation.

Resource names and ownership are deterministic. A preseeded agent report
cannot skip this resource gate. Once resources exist, Public V1 reports can be
accepted and persisted.

Initialization dispatches `InitializeAgentStore` to each exact fresh
Pod/PVC/process identity. The controller accepts schema-14 established reports,
initializes one shared history, and originates the first topology containing
the desired `spec.replicas` members.

## Topology and Session Convergence

Logical membership and accepted process endpoints are stored separately.
Reports are selected by exact replica/storage incarnation rather than logical
replica ID alone.

A same-storage process restart with a temporary replicator identity receives
only contained Open. Once its real creation identity is published, the
controller persists session renewal before any new role, epoch, or
configuration authority. Renewal advances survivor epoch barriers and installs
exact per-member configuration, with secondaries completed before the primary.

Same-cardinality Pod/PVC replacement persists allocation, storage binding,
Build authority, completion, admission, and predecessor retirement as
independent level-triggered stages.

## Planned Switchover

Request a transfer with `spec.switchover.requestId` and
`spec.switchover.targetReplicaId`. Before authorizing transfer work, the
controller persists `publicV1Topology.pendingSwitchover` alongside the starting
configuration and exact admitted sessions. Removing or retargeting the spec
cannot abandon prepared peer epochs. The accepted transfer finishes first,
including after controller recreation; a later request is considered only
afterward. Completion records the frozen request ID, not a newer spec value.
If the source is already unavailable before acceptance, the request does not
block ordinary delayed failover to a surviving requested target.

Each peer prepares the next epoch with the old primary retained. Source Swap
atomically revokes access, catches up the requested successor and demotes before
final target promotion. Final publication clears the pending request. Missing or
changed frozen process/storage participants cause waiting rather than silently
substituting a different transfer.

## Failover

When the primary is unavailable, the controller records its logical ID and the
first unavailable timestamp. It continues exact observation during
`failoverDelaySeconds`; recovery before the deadline clears the marker without
changing topology or routing.

After the deadline, only the unavailable primary is suppressed. The controller
fences survivors at the new epoch, configures secondaries before the promoted
primary, catches up every survivor, persists the new topology, then routes the
write Service. A resumed old primary remains fenced.

## Identity and Fencing

Replica ID, Pod UID, PVC UID, agent generation, process session, epoch,
history, configuration, operation ID, and build attempt are distinct fences.

Primary role alone does not grant writes. Exactly one current provider writer
may exist only after admitted Open, and stale/superseded containment revokes
that ownership.

## Commands and Reports

The controller and runtime use Public V1 protocol version 13. Production
commands are:

- initialization;
- Public V1 history/session renewal;
- Public V1 operation.

Reports contain public role-correct progress, history, storage incarnation,
service location, data-loss state, and completed operations. They do not
expose private ACK sets, copy cursors, quorum positions, or managed-engine
continuation state.

Control RPCs use exact per-Pod port 50051 endpoints. Replication and copy use
port 50052. Status-only persistence reads do not contact agents, while
operation dispatch/report reads observe only the exact target.

## Tests and Diagnostics

Assets are in `kuberic-controller/deploy/`. For an owned disposable cluster:

```bash
just level-triggered-images
just level-triggered-install
just level-triggered-kind-test bootstrap replacement failover
```

See [Testing](testing.md) for the ownership contract and complete commands.

## Unsupported Operations

- existing-state import or migration;
- rolling or mixed-version operation;
- compatibility conversion from removed APIs;
- published release installation;
- production activation or traffic movement;
- Public V1 transport credentials;
- generalized Service Fabric placement and policy parity.

Old state is rejected unchanged. Any future production rollout is separately
designed around fresh state rather than a compatibility mode.
