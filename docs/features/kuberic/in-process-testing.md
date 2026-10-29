# Downstream In-Process Application Tests

**M1 checkpoint:** production reconciliation now requires format-15 semantic
goals and an ordinary-compatible factory. Built-in/KV/SQLite/PG/external-provider
integration explicitly rejects until M2. Historical formation/failover/copy
journeys described below now check exact unavailability where that route moved;
their later clauses do not execute. The [ordinary M1 witness](ordinary-hosting-m1.md#verification-and-limits)
instead runs the real reconciler/agent transport with an independent ordinary
factory, early discovery, retained lifetimes and separate admission.
The reusable adapter/package boundary remains, but the older counter/provider
example is not a supported current production engine.

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

Supply the application's actual owned data/runtime paths with
`CreatedApplication::with_storage_paths`. The host reports the authoritative
Open mode for those bindings: fresh initialization uses `OpenMode::New`, while
reopening initialized storage uses `OpenMode::Existing`. The controller uses
that report rather than inferring a mode from topology or a missing endpoint.
Do not omit storage bindings to force Existing mode. Reports require explicit
Open-mode evidence; older controller/runtime revisions must not be mixed.

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

The default remains three replicas. A bounded formation test can use
`builder(factory).initial_replicas(1).start()`, `settle_members(1)`, then
`scale_to(2)` and `settle_members(2)`. Initial and desired counts are validated
in the harness's supported range of one through three; this is not a production
CRD cardinality restriction. Scaling changes only spec intent. Initialization,
source/target Build and separate admission still run through the real controller.
`settle()` retains its three-member expectation; use `settle_members` for another
requested count.
See [builder:1463–1520](../../../kuberic-controller/src/test_util/cluster.rs#L1463-L1520),
[singleton settle:1639–1654](../../../kuberic-controller/src/test_util/cluster.rs#L1639-L1654)
and [spec-only scaling:2077–2089](../../../kuberic-controller/src/test_util/cluster.rs#L2077-L2089).

`drive` advances reconciliation concurrently with an application future;
`probe` bounds the future without advancing reconciliation. All helpers share
one scenario deadline. A failed or timed-out write can already have a durable
ambiguous outcome; the harness does not retry mutations automatically.

Replica references are scoped to the exact cluster, Pod/PVC and process/
creation session. Contained restart reuses the volume but returns a fresh
reference; replacement follows the controller's real PVC/Pod authority.
Retained old service handles still address the old instance, making real
stale-writer assertions possible.

An RPC outage alone does not remove a still-desired secondary while its exact
Pod remains live and the admitted primary is reachable. Membership is retained
through restart gaps; positive Pod retirement/replacement and explicit
scale-down still permit topology changes.

Finish every normal result path with the consuming async `finish(result)`
operation. It drains the supervisor, startup/host tasks, application workers
and socket owners, preserving scenario and cleanup errors. Drop only requests
shutdown; it cannot acknowledge async completion. Failure diagnostics redact
credentials and retain owned storage locally for inspection. Retained files
can contain application data and credentials and are not uploaded
automatically.

## Fault and Transfer Tests

The pre-M1 harness supported the built-in persisted Public V1 profile and bounded
custom-native formation through the
[custom companion](replicator-boundary.md#custom-controller-integration).
PostgreSQL's [production-engine fixture](../../../examples/postgres/src/public_controller_tests.rs)
owns its actual native coordination listeners, processes and storage.
Controls cover cooperative restart/stop, controller-owned Pod
replacement, support-resource repair, planned switchover and real loopback TCP
isolation. Readiness validates exact accepted/live evidence, access grants,
canonical configuration and settled progress, not only a primary role.

Built-in transport cuts require an acknowledged routed built-in creation.
Custom application transports are not borrowed from that bundle or silently
treated as routable: unsupported transport controls return an explicit
`UnsupportedCapability` error. Native database binaries and application-specific
transport prerequisites remain the application's responsibility.

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

The counter application/provider tests are normal workspace integration
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
providers, arbitrary custom-transport fault injection, or V2 transactional
conformance. Retain provider-specific and owned KinD tests for their existing
claims. See the [design](../../proposal/downstream-in-process-testing.md) and
[test strategy](testing.md).
