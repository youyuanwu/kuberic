# kuberic-controller

Namespace-bound Kubernetes controller for the experimental Public V1 Kuberic
stack.

The controller watches `operator.kuberic.io/v1alpha1` `KubericSet` resources
and their owned Pods, PVCs, Services, and Secrets. It consumes shared Public V1
contracts from `kuberic-runtime` with runtime host features disabled.

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

## Configuration

The binary requires:

- `KUBERIC_INSTALLATION_NAMESPACE`;
- `KUBERIC_AGENT_BEARER_TOKEN`.

The installation namespace must be one DNS label. Kubernetes clients and
watches are namespaced. Deployment RBAC uses `Role`/`RoleBinding`.

## Status and Protocol

CRD status is structurally typed and contains:

- Public V1 authority;
- session renewal;
- transition;
- operation.

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
