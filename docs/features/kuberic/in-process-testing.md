# Downstream In-Process Application Tests

Enable the controller's optional `test-util` feature in development
dependencies to run a production reconciler and real hosted application
replicas without Kubernetes:

```toml
[dependencies]
kuberic-runtime = "=0.0.1"

[dev-dependencies]
kuberic-controller = { version = "=0.0.1", features = ["test-util"] }
```

Use matching versions and Cargo sources for both crates. Source consumers
should use the same Git revision or consistent workspace patches for both.
These dependency declarations describe the API, not a promise that a release
with this feature is already available on crates.io.

The `test-util` feature is disabled by default. It enables runtime hosting and
testing helpers without selecting a fake client in production entry points.
The application must explicitly construct `kuberic_controller::test_util::TestCluster`.
Rust and `protoc` remain compilation prerequisites; a cluster, kubeconfig,
container runtime and application executable are not required.

## Application Adapter

Implement `ApplicationFactory` in the application's test-support module:

- `create` receives the exact replica identity, resolved application
  configuration, owned application directory and local replication address.
  Construct an inert `StatefulServiceReplica`, classify its actual storage as
  `FreshEmpty` or `Established`, and return `CreatedApplication`.
- `shutdown_and_drain` stops and joins application-owned workers after the
  host/replicator has drained. It must retain task failures and remain usable
  after service Close or Abort.

Use the production application/provider implementation. Do not call Open or
change role/access in the adapter: the production controller and host own
those actions. Unexpected, incompatible or partial provider state must reject
unchanged rather than be erased or classified as empty.

The independent [counter application](../../../scripts/fixtures/controller-test-util/src/lib.rs),
[factory](../../../scripts/fixtures/controller-test-util/src/harness.rs) and
[journeys](../../../scripts/fixtures/controller-test-util/src/journeys.rs) form
a complete downstream example. It has its own provider, retained history,
snapshot/copy behavior, file durability and operation-stream consumers; it
does not depend on KVStore2 or repository-private host capabilities.

## Scenario Ownership

Use a multithreaded Tokio test runtime. Create a cluster from the factory and
symbolic image, then call `settle` to drive real bootstrap and admission.
Obtain the primary's exact reference from that observation and use `service`
to call the application's own request API. There is no harness-defined
key/value or transaction protocol.

`drive` advances reconciliation concurrently with an application future;
`probe` bounds the future without advancing reconciliation. All helpers share
one scenario deadline. A failed or timed-out write can already have a durable
ambiguous outcome; the harness does not retry mutations automatically.

Replica references are scoped to the exact cluster, Pod/PVC and process/
creation session. Contained restart reuses the volume but returns a fresh
reference; replacement follows the controller's real PVC/Pod authority.
Retained old service handles still address the old instance, making real
stale-writer assertions possible.

Finish every normal result path with the consuming async `finish(result)`
operation. It drains the supervisor, startup/host tasks, application workers
and socket owners, preserving scenario and cleanup errors. Drop only requests
shutdown; it cannot acknowledge async completion. Failure diagnostics redact
credentials and retain owned storage locally for inspection. Retained files
can contain application data and credentials and are not uploaded
automatically.

## Fault and Transfer Tests

The harness supports the built-in persisted Public V1 profile with three
replicas. Controls cover cooperative restart/stop, controller-owned Pod
replacement, support-resource repair, planned switchover and real loopback TCP
isolation. Readiness validates exact accepted/live evidence, access grants,
canonical configuration and settled progress, not only a primary role.

There is one outstanding transport-fault owner per cluster, shared by manual
group cuts and armed Build-dial faults. A second cut is rejected before
mutation, even for disjoint hosts. Cut entry and healing acknowledge actual
stream/gate effects. Token Drop requests restoration but keeps the reservation
until cleanup is acknowledged. A group cut supports quorum-loss scenarios
without concurrent independent tokens.

Typed switchover submission/clear and retained pause/resume controls support
deterministic intent/completion cuts. While paused, use `probe` for a real
application write, inspect detached observations, update only the requested
spec, or recreate the controller without releasing the cut. Accepted frozen
authority remains controller-owned. No raw Kubernetes client, reconciler,
agent store or authority-mutating host handle is exposed.

## Archive and Source Checks

```bash
cargo nextest run -p kuberic-level-tests --all-features \
  --test downstream_test_util --profile ordinary
just nextest-source
```

The eight counter application/provider tests are normal workspace integration
tests. They are discovered by nextest, included in the reusable archive and
executed in its ordinary partitions. They share the self-contained counter
source with the standalone consumer fixture rather than running through a
Python wrapper.

The separate source-check job has a checkout and build tools. Its ignored Rust
`source_checks::packaged_controller_test_util_boundary` gate creates controller
and runtime packages and an isolated Cargo directory source containing their
packaged dependency closure. A consumer outside repository Cargo configuration,
with `KUBERIC_WORKSPACE_TESTS` unset, checks feature-off/private/conformance
boundaries and compiles the shared behavior tests against packaged crates.
Workspace path dependencies alone are not package-safety evidence.

All nextest test binaries, including source gates, are archived. Source checks
still run in the checkout-backed job, not an archive-only runtime partition.
That job also runs doctests, which are not nextest archive targets. The package
gate proves compilation and dependency closure; actual application behavior is
proved by the archived Rust tests, not claimed from a compiler-only check.

Runtime `testing` keeps reusable fixtures available to downstream consumers.
Checkout-bound source inventories and executable conformance helpers require
the separate repository-owned configuration. Never set
`KUBERIC_WORKSPACE_TESTS` in a downstream test environment; even a value of
`0` requests that configuration.

## Evidence Limits

This is real application/agent/replication/storage integration inside one
process, not a simulated successful provider or controller. Kubernetes
resource storage and Pod/PVC realization are narrow test implementations.
Restart is cooperative shutdown/reconstruction, not a process crash.

The harness does not establish Kubernetes admission, RBAC, watch delivery,
DNS/Service packet routing, image startup, application HTTP serialization,
power-loss durability, native SF parity, arbitrary network faults, volatile
providers, custom-replicator transport support, or V2 transactional
conformance. Retain provider-specific and owned KinD tests for their existing
claims. See the [design](../../proposal/downstream-in-process-testing.md) and
[test strategy](testing.md).
