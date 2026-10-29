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

1. validates required format-15 spec/retained state and observes namespaced resources;
2. obtains callback-free exact agent snapshots;
3. converges missing support/replica resources;
4. retains semantic replica goals and accepts exact achieved outcomes;
5. requeues for observation.

Fresh bootstrap creates credentials, peer/write Services, PVCs, Pods, and exact
per-Pod endpoints before consuming reports. A preseeded report cannot skip
resource convergence.

Independent per-replica Discover/Open preparation permits all endpoints to
publish before held peer-dependent recovery finishes. Target PrepareIdle precedes
ordinary source Build; exact two-ended Ready acknowledgement precedes separate
joint/current voting admission and catch-up. Local agents choose ordered
callbacks, not the controller. Interrupted matching-format work stays uncertain;
automatic retained-history election, contraction and primary replacement are
explicitly unavailable or waiting, not inferred from stale outcomes.

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
- typed canonical per-replica goals, digests and exact outcomes.

Status-only reads avoid agent RPCs. Operation dispatch/report paths observe
only the exact target; ordinary reconciliation retains the broader observation
needed for topology and failover decisions.

Host/control/CRD format 15 is required, including `spec.hostingFormat: 15`.
Old/missing/mixed formats and retained command/renewal/replacement state reject
before mutation; there is no converter or data reset. Fresh-only deployment
policy does not remove matching-format restart/SQLite rollback recovery.

The [M1 controller/agent/host split](../docs/features/kuberic/ordinary-hosting-m1.md)
is implemented without target engine grant dispatch or mandatory companion/
diagnostics. Built-in/KV/SQLite/PostgreSQL production integration is explicitly
unavailable pending M2; comprehensive recovery qualification remains M3.

## Deployment

Assets are under `deploy/`. They are development/CI assets, not published
release targets.

The retained default-engine cluster recipes are not supported M1 deployment
instructions. M1 acceptance uses an independent ordinary factory and real agent
transport against an in-process Kubernetes storage model, not live KinD.

There is no migration, compatibility mode, Kubernetes cutover Lease, image
publication, or production-activation procedure.

See the [operator guide](../docs/features/kuberic/level-triggered-operator.md)
and [test strategy](../docs/features/kuberic/testing.md).
