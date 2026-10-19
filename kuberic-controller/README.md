# kuberic-controller

Namespace-bound Kubernetes controller for the experimental Public V1 Kuberic
stack.

The controller watches `operator.kuberic.io/v1alpha1` `KubericSet` resources
and their owned Pods, PVCs, Services, and Secrets. It consumes shared Public V1
contracts from `kuberic-runtime` with runtime host features disabled.

## In-Process Application Tests

The optional `test-util` feature exposes `test_util::TestCluster` and
`ApplicationFactory` for downstream application integration tests. It hosts
real application replicas and drives the production reconciler against an
owned Kubernetes resource test service. It is disabled by default and never
redirects production controller entry points.

Use the controller as a dev-dependency with `features = ["test-util"]` and a
matching runtime dependency. See the
[downstream testing guide](../docs/features/kuberic/in-process-testing.md) for
factory/lifecycle ownership, typed faults, package checks and evidence limits.

## Reconciliation

Each reconcile:

1. observes the complete namespaced resource set and agent reports;
2. validates accepted Public V1 status;
3. converges missing support/replica resources;
4. applies history, session-renewal, operation, or evidence work;
5. requeues for observation.

Fresh bootstrap creates credentials, peer/write Services, PVCs, Pods, and exact
per-Pod endpoints before consuming reports. A preseeded report cannot skip
resource convergence.

After bootstrap, the controller persists exact topology sessions separately
from logical membership. A same-storage process restart performs only a
contained Open while its replicator identity is temporary, then originates
session renewal before role, epoch, or configuration authority. Replacement
planning selects the durably bound incarnation even while predecessor and
replacement reports overlap.

Main observation probes an unavailable primary until
`failoverDelaySeconds` expires. After the deadline it omits only that declared
primary, configures surviving secondaries before the promoted primary, catches
up all survivors, and publishes the new topology before routing writes.

## Configuration

The binary requires:

- `KUBERIC_INSTALLATION_NAMESPACE`;
- `KUBERIC_AGENT_BEARER_TOKEN`.

The installation namespace must be one DNS label. Kubernetes clients and
watches are namespaced. Deployment RBAC uses `Role`/`RoleBinding`.

## Status and Protocol

CRD status is structurally typed and contains:

- Public V1 authority;
- topology;
- session renewal;
- replacement;
- transition;
- operation.

Status-only reads avoid agent RPCs. Operation dispatch/report paths observe
only the exact target; ordinary reconciliation retains the broader observation
needed for topology and failover decisions.

Protocol version 11 is the only production command/report path. Old and
partial controller state rejects before planning or mutation.

## Deployment

Assets are under `deploy/`. They are development/CI assets, not published
release targets.

```bash
just level-triggered-images
just level-triggered-install
just level-triggered-kind-test bootstrap replacement failover
```

There is no migration, compatibility mode, Kubernetes cutover Lease, image
publication, or production-activation procedure.

See the [operator guide](../docs/features/kuberic/level-triggered-operator.md)
and [test strategy](../docs/features/kuberic/testing.md).
