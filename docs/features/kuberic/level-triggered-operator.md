# Level-Triggered Kuberic Operator

## Status

The `operator.kuberic.io/v1alpha1` stack is the only Kuberic operator path.
It is experimental and intended for development and CI. Images are not
published and no production activation procedure is provided.

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
- `publicV1SessionRenewal`;
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

## Identity and Fencing

Replica ID, Pod UID, PVC UID, agent generation, process session, epoch,
history, configuration, operation ID, and build attempt are distinct fences.

Primary role alone does not grant writes. Exactly one current provider writer
may exist only after admitted Open, and stale/superseded containment revokes
that ownership.

## Commands and Reports

The controller and runtime use Public V1 protocol version 11. Production
commands are:

- initialization;
- Public V1 history/session renewal;
- Public V1 operation.

Reports contain public role-correct progress, history, storage incarnation,
service location, data-loss state, and completed operations. They do not
expose private ACK sets, copy cursors, quorum positions, or managed-engine
continuation state.

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
- primary removal or generalized scaling policy parity with Service Fabric.

Old state is rejected unchanged. Any future production rollout is separately
designed around fresh state rather than a compatibility mode.
