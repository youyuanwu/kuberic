# Downstream In-Process Testing Harness

## Status and Decision

**Implemented, experimental.** The controller/application in-process harness
is exposed as `kuberic_controller::test_util`, enabled by the opt-in `test-util`
Cargo feature. An application factory and typed replica access replace its
original KVStore2-specific construction and assertions. Downstream tests use
their real service through production controller, host, replication and storage
paths. See the [usage guide](../features/kuberic/in-process-testing.md) and its
complete independently packaged application example.

Keep `kuberic-runtime` and `kuberic-controller` as the two production crates.
Downstream tests add `kuberic-controller` as a development dependency with
`test-util`; no third crate, production runtime mode or alternative controller
is needed. Do not change `Replicator`, `PrimaryReplicator`, or `StateProvider`.

This extends the [implemented harness architecture](in-process-system-test-harness.md),
whose original private, fixed-KVStore2 scope was appropriate for its first
implementation. The sections below retain the reviewed design, extraction
decisions and acceptance requirements; they do not claim a published release,
production readiness or conformance beyond the stated test boundary.

## Current Implementation and Extraction Boundary

The following evidence is from repository revision `c0e52ee`.

| Current surface | Consequence for downstream use |
|---|---|
| [`kuberic-level-tests/Cargo.toml`](https://github.com/youyuanwu/kuberic/blob/c0e52ee6ef361d86cf9e984e1f0682bc1f479013/kuberic-level-tests/Cargo.toml) and [`src/lib.rs`](https://github.com/youyuanwu/kuberic/blob/c0e52ee6ef361d86cf9e984e1f0682bc1f479013/kuberic-level-tests/src/lib.rs) | The package has `publish = false`; the harness is a private `cfg(test)` module with dev-only dependencies. Adding it as a dependency does not expose a library. |
| [`Harness::new`](https://github.com/youyuanwu/kuberic/blob/c0e52ee6ef361d86cf9e984e1f0682bc1f479013/kuberic-level-tests/src/in_process/harness.rs#L114) | Namespace, resource name, three replicas, image, timeouts and KVStore2 construction are fixed. |
| [`pod::resolve`](https://github.com/youyuanwu/kuberic/blob/c0e52ee6ef361d86cf9e984e1f0682bc1f479013/kuberic-level-tests/src/in_process/pod.rs#L14) | The resolver validates the generated Pod/environment/PVC relationship, but accepts only the KVStore2 image and translates its local configuration. |
| [`supervise`](https://github.com/youyuanwu/kuberic/blob/c0e52ee6ef361d86cf9e984e1f0682bc1f479013/kuberic-level-tests/src/in_process/harness.rs#L910) | Host lifecycle, application construction, storage classification, routing registration and infrastructure publication are coupled. |
| [`Harness::ready_observed`](https://github.com/youyuanwu/kuberic/blob/c0e52ee6ef361d86cf9e984e1f0682bc1f479013/kuberic-level-tests/src/in_process/harness.rs#L295) and [`configurations_settled`](https://github.com/youyuanwu/kuberic/blob/c0e52ee6ef361d86cf9e984e1f0682bc1f479013/kuberic-level-tests/src/in_process/harness.rs#L506) | Readiness already requires exact accepted/live identity and canonical configuration evidence, not just a reported primary. Preserve this shared oracle. |
| [`Harness::write`](https://github.com/youyuanwu/kuberic/blob/c0e52ee6ef361d86cf9e984e1f0682bc1f479013/kuberic-level-tests/src/in_process/harness.rs#L717) and value assertions | Request and data assertions are KVStore2-specific; they belong in application tests, not the reusable core. |
| [`Harness::finish`](https://github.com/youyuanwu/kuberic/blob/c0e52ee6ef361d86cf9e984e1f0682bc1f479013/kuberic-level-tests/src/in_process/harness.rs#L807), [`Hosted::stop`](https://github.com/youyuanwu/kuberic/blob/c0e52ee6ef361d86cf9e984e1f0682bc1f479013/kuberic-level-tests/src/in_process/harness.rs#L56) | The harness owns asynchronous host/application/network cleanup and retains failed storage. A downstream API must preserve both scenario and cleanup failures. |
| [`kube.rs`](https://github.com/youyuanwu/kuberic/blob/c0e52ee6ef361d86cf9e984e1f0682bc1f479013/kuberic-level-tests/src/in_process/kube.rs) and [`transport.rs`](https://github.com/youyuanwu/kuberic/blob/c0e52ee6ef361d86cf9e984e1f0682bc1f479013/kuberic-level-tests/src/in_process/transport.rs) | The limited Kubernetes service and owned socket proxies are reusable implementation details, not general-purpose public mock APIs. |

There is already an opt-in
[`kuberic_runtime::testing`](https://github.com/youyuanwu/kuberic/blob/c0e52ee6ef361d86cf9e984e1f0682bc1f479013/kuberic-runtime/src/testing/mod.rs)
surface. Its detached fixtures and low-level conformance helpers are not this
controller-driven harness. Existing testing-feature host methods can issue
fixture authority; the new module must not wrap those as ordinary scenario
controls.

At that baseline, the runtime `testing` feature was not package-safe:
[`public_v1_conformance.rs:140-165`](https://github.com/youyuanwu/kuberic/blob/c0e52ee6ef361d86cf9e984e1f0682bc1f479013/kuberic-runtime/src/testing/public_v1_conformance.rs#L140-L165)
unconditionally includes sibling controller sources. The
[package-safe testing prerequisite](#package-safe-runtime-testing-prerequisite)
below records the required separation and qualification rather than treating
checkout fixtures alone as package-safety evidence.

## Goals and Initial Support

The first downstream release supports persisted applications using the current
built-in Public V1 replicator, implementing `StatefulServiceReplica`, and
constructible as a Rust library. Each test uses one application factory and
three replicas. The minimum useful journeys are bootstrap, a real application
write/read, contained restart, secondary replacement, stopped-primary failover,
live-primary isolation/return, quorum loss/restoration and planned switchover.

The reusable harness owns infrastructure and observations. The application owns
its provider, serialization, operation-stream consumers, client request API and
data assertions. There is no generic key/value API, success receipt format or
application-state comparison invented by the harness.

The initial release does not promise custom-replicator transport faults,
volatile-service scenarios, arbitrary replica counts, scaling, multiple
applications in one cluster, child processes or SQLite/PostgreSQL adapters.
Those require separate capability and coverage work. In particular,
`TestingTcpRoute` redirects the built-in replication/Build dials; it does not
intercept arbitrary application sockets or PostgreSQL streaming.

No cluster, container runtime, kubeconfig or application executable is needed.
Always construct `kube::Client` from the owned test service; never load ambient
cluster credentials or fall back to a real API server.
Compilation still needs the runtime/controller build prerequisites, including
the pinned supported Rust toolchain and `protoc`; "cluster-free" is not
"dependency-free."

## Feature, Module and Dependency Layout

```text
downstream application
  dependencies     -> kuberic-runtime
  dev-dependencies -> kuberic-controller [test-util]
                         +-> kuberic-runtime [testing -> host]
                         +-> existing controller dependencies
                         +-> optional harness dependencies / owned TCP proxies

kuberic-level-tests [dev-dependency] -> kuberic-controller [test-util]
                                  -> kvstore2 (local test adapter)
```

Add a feature-gated public module to the existing controller library:

```rust
#[cfg(feature = "test-util")]
pub mod test_util;
```

Do not use `cfg(test)` for the exported module: downstream Cargo does not
compile a dependency's test modules or dev-dependencies. Reuse the controller's
existing normal dependencies, and make additional harness dependencies
optional normal dependencies activated by `test-util`. The current extraction
needs `tempfile`, Tower with `util`, `http`, `form_urlencoded`, UUID with `v4`,
and `anyhow` if retained for internal error handling.

`test-util` enables `kuberic-runtime/testing`, which already enables `host`.
It does not need a second runtime feature named `test-util`, a dependency from
the runtime back to the controller, or the existing `runtime-test-bridge`.
That bridge remains separate repository conformance plumbing, not the
downstream scenario API.

### Package-Safe Runtime Testing Prerequisite

Separate reusable runtime fixtures from checkout-bound conformance machinery
before enabling the downstream controller harness. Keep the package-safe
session/state/store helpers and required host/transport test seams under
`kuberic-runtime/testing`. Compile the existing
`public_v1_conformance` and `public_v1_alternate_registration_fixture` modules,
and their conformance re-exports, only under the repository-owned
`kuberic_workspace_tests` configuration as well as `testing`.

Reuse the existing repository mechanism rather than adding a Cargo feature:
[`.cargo/config.toml:1-3`](https://github.com/youyuanwu/kuberic/blob/c0e52ee6ef361d86cf9e984e1f0682bc1f479013/.cargo/config.toml#L1-L3) sets
`KUBERIC_WORKSPACE_TESTS`, and
[`kuberic-runtime/build.rs:4-8`](https://github.com/youyuanwu/kuberic/blob/c0e52ee6ef361d86cf9e984e1f0682bc1f479013/kuberic-runtime/build.rs#L4-L8)
maps its presence to the checked custom configuration. `--all-features` must
not enable repository conformance. The build script must diagnose a requested
repository configuration without its required sibling sources/workspace;
do not silently disable requested checks or compile success-shaped stubs.
The public controller `test-util` feature must never set this environment
variable or depend on conformance exports.

Gate the runtime conformance integration-test entry point on the same
repository configuration. Preserve its actual source inventories, alternate
registration rejection, nested executable gate, and
[`examples/postgres` conformance calls](https://github.com/youyuanwu/kuberic/blob/c0e52ee6ef361d86cf9e984e1f0682bc1f479013/examples/postgres/src/lib.rs#L26-L74)
in explicitly repository-owned test invocations. Account for these callers,
the API inventories and the
[nextest conformance selectors](https://github.com/youyuanwu/kuberic/blob/c0e52ee6ef361d86cf9e984e1f0682bc1f479013/.config/nextest.toml#L26-L29)
when separating the surfaces; do not remove their assertions merely to make a
packaged library compile. This does not require a new crate, copied controller
sources, or access to private runtime capabilities from another package.

The prerequisite's acceptance gate builds a packaged runtime with `testing`
and with all features, without a sibling controller checkout. Run from an
isolated consumer directory outside repository Cargo configuration, with
`KUBERIC_WORKSPACE_TESTS` **unset**, not set to `0` (the current build script
checks presence). Packaged dependencies must resolve from the isolated
registry/package closure, not a workspace path or patch fallback. The reusable
fixtures must compile; conformance helpers must be absent from that consumer's
API. Separately require the existing repository conformance/privacy gates to
remain present and meaningful with the repository configuration enabled.
Only then proceed to packaged controller `test-util` consumers.

### Feature Isolation and Packaging

Keep the controller's default features unchanged. Without `test-util`, the
module and its additional dependencies are absent. With it enabled, including
under `--all-features`, production entry points still use their existing
controller/client paths: feature unification must not redirect production
behavior into a fake cluster. Only explicit harness construction creates the
owned Kubernetes test service.

The controller is the natural home because it already depends on the runtime
and owns the reconciliation layer under test. Hosting the complete harness
inside the runtime would add a normal dependency cycle; making the controller
dependency optional would not make that cycle valid. Keep runtime-only
fixtures under `kuberic_runtime::testing`. Keeping the full harness in
`kuberic-level-tests` would instead couple supported use to an unpublished
repository test package.

Keep runtime and controller release versions aligned and initially pin the
controller's runtime dependency exactly. Their shared Rust types and protocol
behavior must agree; Cargo feature unification must not select a second runtime
package identity behind an application service. External fixtures must
exercise the dependency declarations. This is an experimental testing API
released with the controller, not an independently versioned package or stable
protocol abstraction.

Consumers must use the same Cargo source identity as well as version: do not
mix a registry runtime with a Git/path copy pulled by the controller. Document
matching dependency declarations and consistent workspace patches for source
builds. The test module needs no runtime re-export facade or alternative
service trait to work around duplicate package identities.

Source consumption may use matching Git revisions before registry
distribution is available. Registry readiness additionally requires versioned
dependencies, inclusion of generated-code inputs, and successful packaged
consumer checks. Do not advertise a crates.io installation until the package
and its dependency closure are actually available. Published-crate and image
distribution remain separate decisions.

Keep implementation modules private behind reviewed `test_util` re-exports:

```text
kuberic-controller/src/test_util/
  mod.rs             reviewed re-exports
  application.rs     factory, context and constructed application
  cluster.rs         builder, driver, replica references and observations
  faults.rs          bounded typed controls and owned tokens
  error.rs           classified failures and redacted diagnostics
  supervisor.rs      private host/application ownership
  kube.rs            private limited Kubernetes service
  transport.rs       private socket proxy implementation
```

This is an extraction, not a second implementation. After migration there is
one reusable resource store, supervisor, reconciliation driver and readiness
oracle. Repository-only adversarial tests may access private implementation
through the module's unit tests; public journeys must use its public API.
Do not add a controller dependency on examples, `kuberic-level-tests`, or
repository fixture source paths. Co-location with controller internals is not
permission to bypass public reconciliation/cluster APIs or reach into private
authority/registration machinery.

## Application Construction Contract

The factory lives under `test_util`. The following shows its API shape; the
usage guide links the complete compilable adapter and application:

```rust
#[async_trait::async_trait]
pub trait ApplicationFactory: Send + Sync + 'static {
    type Service: kuberic_runtime::StatefulServiceReplica + 'static;

    async fn create(
        &self,
        context: ReplicaContext,
    ) -> Result<CreatedApplication<Self::Service>>;

    async fn shutdown_and_drain(&self, service: Arc<Self::Service>) -> Result<()>;
}
```

`Result` is `test_util::Result`, distinct from the production controller's
result type. `ReplicaContext` has read-only accessors for the exact replica
identity, application-owned directory,
resolved application environment (excluding agent-control credentials) and
local replication address. It supplies
`http://127.0.0.1:0` for the initial built-in engine profile. It does not let the
factory override Pod/PVC identity, control credentials, controller authority or
agent storage.

`CreatedApplication<S>` contains an `Arc<S>`, the existing
`ApplicationStorageState`, and application storage-path bindings for
`ReplicaHost::with_application_storage_paths`. Constructors validate that
bindings remain inside the owned application directory. Preserve the existing
KVStore2 layout while extracting it; the public factory must not depend on the
agent database filename.

The factory must classify its actual on-disk state. An empty directory is not
evidence that an existing provider format is valid, and an incompatible or
partially written provider must return an error rather than be erased or
reported as fresh. The host retains its own independent initialization and
storage checks. PVC UID owns the volume; restart reuses it, replacement follows
the controller's declared claim, and a fresh PVC gets fresh storage.

`create` constructs an inert service: no detached tasks or externally visible
listeners before it returns. Failed or cancelled construction must release
partially acquired synchronous resources. Application background work begins
under the service lifecycle and retains joinable ownership. An application
needing pre-Open background startup falls outside this initial contract rather
than being given an unowned task escape hatch.

The supervisor constructs `ReplicaHost` and owns the control listener and
startup future. The factory must not call service Open, install configurations,
apply provider operations or manufacture access grants. Those remain production
host/controller actions. Normal application construction, not a test-specific
provider implementation, should be reused.

After host shutdown and drain, `shutdown_and_drain` must stop and join any
application-owned workers still retained after Close/Abort. It is terminal,
repeatable, and preserves worker errors and panics. It cannot imply retirement
or delete provider data. This hook is deliberately separate from the protected
production traits; KVStore2 already has the corresponding
[`shutdown_and_drain`](https://github.com/youyuanwu/kuberic/blob/c0e52ee6ef361d86cf9e984e1f0682bc1f479013/examples/kvstore2/src/service.rs#L87).

### Pod Resolution and Image Binding

`TestCluster::builder(factory)` registers exactly one symbolic image string.
The controller still creates the real `KubericSet`, Pod, Service, Secret and PVC
objects. The resolver validates the observed Pod's actual environment,
downward-API values, Secret references, volume and identity before invoking the
factory. Only local addresses and mounted paths are translated.

Generalize the current resolver's KVStore2-specific image check and error
wording, not its identity validation. The image string maps to the registered
factory; no image is pulled. Other images, custom commands, unsupported
environment sources, unsafe mounts and unsupported API requests fail
explicitly. Do not bypass the generated Pod using a hand-written process
configuration. Additional application-specific settings belong in the factory
or a future explicit controller Pod-template API, not an invisible mutation of
the production-generated Pod.

## Public Scenario Surface

Expose `TestCluster<F>` with a small builder: application image, namespace/name,
scenario timeout and teardown timeout. Default to the existing three-replica,
one-second failover profile and real time. Validate builder input before
starting work; do not expose a replica-count knob without implementing and
testing its readiness/quorum semantics.

| Proposed operation | Contract |
|---|---|
| `builder(factory).start().await` | Create isolated resources and start the supervisor. Initialization can serve before full host startup; do not wait for topology admission inside construction. Partial startup failure enters owned teardown. |
| `settle().await` | Drive reconciliation until the default three-member readiness predicate holds; return an immutable topology observation containing the primary and members. |
| `settle_members(expected).await` | Require the same exact readiness for two or three admitted members, including two-member failover after an acknowledged host stop. This observes membership; it does not change desired replicas or grant quorum. |
| `settle_survivors(&isolation, expected).await` | Use an acknowledged isolation token to exclude only its exact isolated instances; never turn missing reports into healthy evidence. |
| `service(&replica).await` | Return `Arc<F::Service>` for an exact currently live reference. Tests invoke their own application's request API directly. |
| `observe().await` | Return detached, read-only resource, topology and normalized agent observations with identities and progress; no mutable host/store handles. |
| `drive(future).await` | Poll an application/probe future concurrently with the explicit reconciliation driver, bounded by the same scenario deadline and fatal checks. |
| `restart(&replica).await` | Cooperatively stop and drain a live host/application, reconstruct the same Pod/PVC, and drive admission of the new session; return a fresh reference. |
| `stop(&replica).await` | Stop and drain without deleting its Pod or allowing the supervisor to respawn it. Surviving members can fail over. |
| `delete_replica(&replica).await` | Request exact UID-preconditioned Pod deletion; the production controller owns replacement, Build and admission. |
| `delete_support_resource(kind).await` | Delete the observed exact peer Service or credentials Secret incarnation so the production controller repairs it; no arbitrary resource/status write API. |
| `submit_switchover(&target).await` / `clear_switchover(&request).await` | Write only the real spec and return the new request identity or clear the exact current request. Do not drive reconciliation; submitting another target creates a distinct request, not an edit to accepted authority. |
| `switchover(&target).await` | Convenience for submission plus driving to completion of that exact request; never direct role calls. |
| `pause_at(barrier).await` / `resume(pause).await` | Drive to a typed persisted-intent/completion boundary, cancel and join the held controller call, and return an owned pause token; retain the reply hold until explicitly resumed. |
| `probe(future).await` | Bound an application/assertion future by the shared deadline and fatal checks without driving reconciliation, including while paused. |
| `recreate_controller().await` | Cancel/await any owned in-flight reconcile before rebuilding over unchanged resources and hosts. |
| `isolate(&replicas).await` / `heal(token).await` | Acknowledge real stream cuts/restoration for the bounded built-in transport profile; retain the original live hosts. |
| `finish(result).await` | Consume the cluster, check fatal state, drain everything, and combine scenario and teardown errors. |

`ReplicaRef` is opaque and includes a cluster-instance nonce and the exact
resource/replica, Pod/PVC, process-session and replicator-creation identities.
Reject a reference from another cluster or an obsolete incarnation/session;
do not silently resolve it to the latest host with the same replica number.
Before completed startup, expose infrastructure observations rather than
manufacturing a usable replica reference.

Resource observations also carry opaque cluster-scoped `ResourceRef` values
for exact UID-preconditioned deletion holds and fault targeting before host
startup. These are not service handles or admission evidence. Revalidate their
UID on use; a reused Pod/PVC name must not redirect a pending fault.

Old `Arc<Service>` values may intentionally remain in a test for stale-writer
assertions. They continue to address the old real service and must observe its
closed/fenced access; the harness must not replace them with a fresh service.
Direct service access is not authorization or a promise that an earlier
readiness observation is still current.

Use an `Isolation` token for a group so quorum-loss tests can cut two members
together. It records cluster and exact host ownership; unknown/stale references
are errors. Reject restart of an isolated member until the token has been
healed in the initial API.

**Allow only one outstanding transport-fault owner per cluster.** A manual
group isolation and an armed next-Build-dial cut reserve the same exclusive
slot. Reject any second request, even for disjoint members, with
`FaultAlreadyActive` before changing gates or replacing an armed fault.
Validate the entire group and its capabilities before installing a cut; an
invalid member must not leave a partial group isolation. A group token, not
multiple concurrent tokens, supplies the supported quorum-loss case.

An armed Build fault retains the same owner when it enters the cut; it does
not mint an independent isolation. Its entry notification is acknowledged
only after affected existing streams have terminated and new connections are
gated. Survivor waits require the current owner's entered, acknowledged cut,
not merely an armed token or an old boolean-set membership.

`heal` cancels a not-yet-entered fault or restores an entered cut and joins the
relevant gate acknowledgements before releasing the slot. Dropping a token
requests the same owned cleanup; the slot remains occupied while restoration
is pending. A new fault cannot start in that interval. Scope restoration to
the owner ID so a delayed stale-token action cannot affect a later fault.
Transport failure during cut/restoration is fatal and retains uncertain
ownership for teardown; it must not return a success or a free slot.
Teardown cancels armed faults, clears cuts and awaits their owners.

The current
[`Network::isolate`](https://github.com/youyuanwu/kuberic/blob/c0e52ee6ef361d86cf9e984e1f0682bc1f479013/kuberic-level-tests/src/in_process/transport.rs#L170-L187)
only inserts/removes identities from a set, and the
[next-dial hook](https://github.com/youyuanwu/kuberic/blob/c0e52ee6ef361d86cf9e984e1f0682bc1f479013/kuberic-level-tests/src/in_process/transport.rs#L142-L149)
mutates that same set. Add the exclusive ownership contract above rather than
wrapping those booleans with independently healable public tokens. Nested or
concurrent transport-fault schedules are deliberately outside initial scope.

Do not initially expose `kube::Client`, raw JSON status editing, `Reconciler`,
`ReplicaHandle`, fixture-authority helpers, agent database handles, routing
registrations or arbitrary supervisor callbacks. The harness is not a security
sandbox against an application's own filesystem access; this boundary prevents
its supported API from accidentally bypassing the protocol being tested.

### Downstream Test Shape

Illustrative proposed usage; `CounterFactory` and the counter request methods
are supplied by the downstream application's test-support module. Once a
matching release exposes the feature, downstream dependency declarations are:

```toml
[dependencies]
kuberic-runtime = "=0.0.1"

[dev-dependencies]
kuberic-controller = { version = "=0.0.1", features = ["test-util"] }
```

The version is illustrative, not a claim that a registry package with this
feature is available. The test imports the controller testing module:

```rust
use kuberic_controller::test_util::TestCluster;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn acknowledged_state_survives_restart() -> anyhow::Result<()> {
    let mut cluster = TestCluster::builder(CounterFactory::new())
        .image("in-process/my-counter")
        .start()
        .await?;

    let result: anyhow::Result<()> = async {
        let topology = cluster.settle().await?;
        let primary = cluster.service(topology.primary()).await?;
        cluster.drive(async {
            primary.increment(7).await?;
            Ok::<_, anyhow::Error>(())
        }).await?;

        let secondary = topology.secondaries().next().unwrap().clone();
        let replacement_session = cluster.restart(&secondary).await?;
        let reopened = cluster.service(&replacement_session).await?;
        cluster.drive(async {
            anyhow::ensure!(reopened.read().await? == 7);
            Ok::<_, anyhow::Error>(())
        }).await?;
        Ok(())
    }.await;

    cluster.finish(result).await?;
    Ok(())
}
```

The API accepts application errors as sources rather than requiring the
application to return a runtime error. `drive` and `finish` accept fallible
futures/results with errors convertible to the harness's boxed scenario-error
representation; the sketch uses `anyhow` only for the consumer.

Never retry a mutation automatically after an error, deadline, failover or
stale reference. A failed write can already have a durable ambiguous suffix.
The application chooses operation IDs, idempotence/retry policy and how to
compare recovered outcomes. Read/data assertions are application predicates,
not an inferred consequence of matching agent progress.

Prefer a returned `Result` and `finish` to assertions that unwind past async
cleanup. The initial API does not promise async cleanup after arbitrary panic,
process termination or Tokio-runtime destruction. Drop requests cancellation
and retains diagnostics/storage; it is not successful completion evidence.

## Reconciliation, Readiness and Fault Semantics

Retain one explicit driver, not a second background watch/controller runner.
`settle`, lifecycle waits and `drive` use that driver; while none is being
polled, hosts and supervisor continue but reconciliation does not advance.
`drive` takes a future built from cloned service handles, so it need not
borrow the cluster inside that future. It is not a general concurrent control
API: serialize scenario mutations.

Register change notifications before reconciliation, honor successful
requeue policy, and record/retry reconcile errors with nonzero backoff.
Unsupported Kubernetes requests, invalid fixtures, unexpected task exits and
proxy/supervisor failures remain latched fatal errors; reconciliation retry
must not conceal them. Check fatal state before returning a successful wait,
after a supplied future completes, and at finalization.

There is one monotonic scenario deadline, not a fresh timeout per retry or
helper. Bound application futures, observation RPCs and reconcile calls by its
remaining budget. On deadline, cancel scenario work and enter the separately
bounded teardown phase. Dropping an application future is not an operation
rollback; preserve that distinction in the returned diagnostic.

Keep real time: production observations use `SystemTime`, and applications can
own OS threads. Do not offer Tokio-paused time as a whole-system clock or edit
persisted timestamps to force failover.

Shared readiness preserves the current exact-session/configuration checks:
no pending operation, transition, replacement, renewal or transfer; accepted
and live identity/history/storage/session/creation agreement; real Open
endpoints; canonical configuration/peer evidence; correct read/write grants;
and settled built-in committed/applied progress. Observe excluded live
predecessors for closed access as well as admitted members. A primary role,
member count, open port or status-only topology is insufficient.

Do not export the configuration-evidence collector's `ReplicaHandle` access.
It remains internal and read-only even though some current reads use
testing-feature methods. Do not replace its durable command/completion
correlation with an application callback that can unconditionally claim
readiness.

### Bounded Advanced Controls

Preserve repository recreation/conflict/deletion coverage without exposing
arbitrary mutation. A small `faults` surface may provide one-shot controls:

| Control | Allowed effect |
|---|---|
| Persisted-operation reply barrier | Hold a response after the real status write commits; report the captured operation identity without permitting its alteration. |
| Completion-status conflict | Advance metadata/resourceVersion only and reject one matching status replacement with 409. |
| Deletion hold | Keep an exact already-deleting Pod/PVC observable until the token is released. |
| Next matching Build dial cut | Arm source/destination isolation for exact observed participants before the first real dial; establish ownership and apply the cut in the route hook, not through a preceding assertion read. |

Use typed call-kind/operation matchers, not public JSON paths, string fragments
or arbitrary store closures. Each wait is deadline-bounded; tokens release on
drop without holding the resource-store lock. Controller recreation must
cancel/await its original reconcile and retain committed mutations before
resuming. Tests still require an observed retry and successful convergence.

Provide `recreate_controller_at(barrier).await` for interruption journeys: the
convenience operation composes `pause_at`, controller recreation and `resume`,
returning the captured immutable operation evidence. For scenarios needing
intervening actions, keep the pause token instead. Tokens do not borrow
`TestCluster`, so the caller can observe, submit/clear the switchover spec,
probe a cloned service, or recreate the controller while holding the reply.
Controller recreation while paused must retain the hold, not implicitly
resume it. Other driving helpers reject a paused driver; `resume` releases the
held reply and re-enables the next explicit drive. Dropping the pause requests
release for owned cleanup; it is not successful scenario completion.
Cancellation of a controller call does not cancel or roll back an agent
action already dispatched.

### Deterministic Switchover Regression Migration

Keep all five existing switchover journeys in
`kuberic-level-tests/src/in_process/journeys.rs`, using the public typed API and
its KVStore2 adapter. Do not move them into controller tests with an example
dependency or replace them with final-topology-only assertions.

`submit_switchover` and `clear_switchover` perform resource-version-checked
spec-only updates and preserve status exactly. Clearing requires the current
spec request ID to match the supplied request; it must not erase a newer one.
Submitting a new target assigns a new request ID. Neither operation cancels,
rewrites or replaces a frozen accepted transfer in status.

Typed switchover barriers identify request, exact participant and semantic
stage: peer Epoch intent, peer Configuration completion, or source handoff
intent/durable completion. They must not expose the current operation-ID
string fragments as a public matcher. Because submission does not drive, a
caller can submit, arm a stage hold, and then `pause_at` without racing a
background reconciler.

| Existing journey | Required migration and preserved assertion |
|---|---|
| [`specification_switchover_preserves_data_and_fences_old_writer`](https://github.com/youyuanwu/kuberic/blob/c0e52ee6ef361d86cf9e984e1f0682bc1f479013/kuberic-level-tests/src/in_process/journeys.rs#L1143-L1145) | Pause at the remaining peer's Epoch intent after target preparation; acknowledge a real old-primary write while the prepared target lacks it; resume and require the final target to contain it and the old writer to reject writes. |
| [`switchover_recreation_after_catch_up_resumes_accepted_handoff`](https://github.com/youyuanwu/kuberic/blob/c0e52ee6ef361d86cf9e984e1f0682bc1f479013/kuberic-level-tests/src/in_process/journeys.rs#L1148-L1150) | Preserve the preparation-boundary write above, then pause/recreate/resume at persisted source-handoff intent. Preserve exact command identity and acknowledged values without duplicate effects. |
| [`switchover_recreation_after_durable_handoff_resumes_promotion`](https://github.com/youyuanwu/kuberic/blob/c0e52ee6ef361d86cf9e984e1f0682bc1f479013/kuberic-level-tests/src/in_process/journeys.rs#L1153-L1155) | Preserve the preparation-boundary write, then recreate at durable source-handoff completion before status acceptance; consume that completion once and preserve data and session/storage identities. |
| [`switchover_cancellation_after_peer_preparation_finishes_frozen_request`](https://github.com/youyuanwu/kuberic/blob/c0e52ee6ef361d86cf9e984e1f0682bc1f479013/kuberic-level-tests/src/in_process/journeys.rs#L1158-L1160) | Pause at peer Configuration completion, clear only the spec request, recreate without releasing the hold, then resume. The originally frozen transfer must finish with its original target and request ID. |
| [`switchover_retarget_after_peer_preparation_serializes_frozen_requests`](https://github.com/youyuanwu/kuberic/blob/c0e52ee6ef361d86cf9e984e1f0682bc1f479013/kuberic-level-tests/src/in_process/journeys.rs#L1163-L1165) | At the same held completion, submit a distinct request for the other peer and recreate. Observe completion of the original frozen request before the new one; preserve data, identities and both stale-writer checks. |

The existing
[frozen-spec edits](https://github.com/youyuanwu/kuberic/blob/c0e52ee6ef361d86cf9e984e1f0682bc1f479013/kuberic-level-tests/src/in_process/journeys.rs#L1187-L1238),
[serialized completion assertions](https://github.com/youyuanwu/kuberic/blob/c0e52ee6ef361d86cf9e984e1f0682bc1f479013/kuberic-level-tests/src/in_process/journeys.rs#L1248-L1313)
and
[preparation-boundary write](https://github.com/youyuanwu/kuberic/blob/c0e52ee6ef361d86cf9e984e1f0682bc1f479013/kuberic-level-tests/src/in_process/journeys.rs#L1347-L1393)
are the acceptance baseline. Use `probe` for the write while paused; `drive`
rejects a paused driver rather than advancing reconciliation at the held reply.
Keep exact completion evidence and application/provider assertions available
through detached observations and the actual application service, not mutable
host journals or a raw Kubernetes client.

### Transport Isolation

TCP isolation cuts and joins existing source/destination streams and gates new
connections, including control and both built-in replication/Build dials.
Endpoint ownership is registered before first dial, not discovered by an
assertion. Canonical peer descriptors retain the real Open endpoint; proxy
addresses affect physical dialing only. Unrelated survivor streams remain
intact. Do not report a successful cut for an unsupported transport.

The current `TestingTcpRoute::register` only installs a callback; registration
alone does not prove the application's selected engine uses it. Add a narrow
`testing`-feature local observation from actual built-in engine construction,
bound to the exact host/creation and owned route registration. Require this
acknowledgement before advertising transport-fault support. An application
factory declaration, generic Public V1 identity, or an open control proxy is
not sufficient. Missing integration returns `UnsupportedCapability`, not a
control-only "isolation" success. This needs no production wire field or change
to a protected interface.

These controls expose existing bounded semantics, not an arbitrary fault
scheduler. Terminal Build recovery limitations described in the
[original harness](in-process-system-test-harness.md#failure-model-and-limits)
remain limitations; exporting the harness does not fix those controller paths.

## Ownership, Teardown and Diagnostics

Each cluster owns its resource store, controller call, supervisor, pending
startups, running hosts, factory-created applications, proxies/listeners,
route registrations and temporary root. Use OS-assigned loopback ports with
listeners held continuously. UUID identities and per-cluster storage isolate
parallel tests, including the existing process-global testing-route registry
keyed by exact instance ID. No fixed ports, global default cluster or
concurrency-one test group.

Finalization stops driver admission, requests supervisor shutdown and awaits
it, releases barriers/deletion holds, closes proxy admission and drains
connections, then cancels/joins startup and running host owners and drains
application workers. Retain routing registration ownership until its host's
dialing work has terminated; forget routes before endpoint/storage reuse.
Remove successful temporary storage only after every owner has completed.
Do not await joins while holding registry locks needed by those tasks.

Startup, host-stop and finalization paths must share this ownership discipline.
A timeout must not drop the only `Hosted`/application cleanup owner and then
report successful cleanup. Preserve ownership through cancellation and any
required abort-and-join path. If an application or host cannot terminate within
the teardown budget, fail explicitly and retain storage; do not certify that
resources were reclaimed or that another cluster may reuse them. Drop is only
a shutdown request, not an asynchronous join.

Return a structured error with classification and sources: invalid
configuration/reference, unsupported capability, scenario/application failure,
deadline, fatal harness failure and cleanup failure. Finalization preserves the
original application error plus *all* cleanup failures, rather than replacing
one with another or returning success after partial cleanup.

Attach a bounded diagnostic bundle: last reconciliation error/count,
redacted resource snapshot, topology and agent identity/progress summaries,
pending fault tokens, task-owner completion state and retained-storage path.
Exclude Secret `data`/`stringData`, bearer tokens and resolved sensitive
environment values from Debug, logs and reports. Do not automatically dump
provider contents. Retained application/agent files may still contain data or
credentials: keep them local and do not upload them automatically.

## Implementation and Acceptance Plan

1. **Make runtime testing package-safe.** In
   `kuberic-runtime/src/testing/mod.rs`, separate reusable helpers from the two
   checkout-bound conformance modules and their re-exports using the existing
   repository configuration. Validate requested repository layout in
   `kuberic-runtime/build.rs`; account for
   `kuberic-runtime/tests/public_v1_conformance.rs`, PostgreSQL conformance
   callers, source-public inventories and nextest/build-only gates. Acceptance
   requires isolated packaged runtime `testing`/all-feature builds with
   conformance helpers absent, plus unchanged real repository conformance
   checks. This is a prerequisite for extraction and packaged consumers.
2. **Extract without behavior changes.** After phase 1, move Kubernetes storage,
   transport and generic orchestration into
   `kuberic-controller/src/test_util`, gated by the opt-in feature and optional
   normal dependencies. Include the minimal factory seam needed to remove
   concrete KVStore2 ownership; keep its adapter/assertions in level tests.
   Move internal helper tests with their implementation. Acceptance requires
   every existing journey/assertion to retain its production paths and
   coverage; intentionally update selectors rather than duplicate or drop
   difficult tests.
3. **Add the factory and public boundary.** Build on phase 2's seam; generalize
   image/application construction, storage bindings and application drain.
   Add opaque scoped references, observations, bounded driver/fault controls
   and classified failures, including actual built-in transport-integration
   evidence. Retain private authority and host internals; update reviewed API
   inventories only for intentional testing APIs, not protected traits.
   Implement the five named switchover migrations and exclusive transport-fault
   owner contract above. Their original application/effect assertions and
   feature-on public API/privacy fixtures are this phase's acceptance gates.
4. **Prove external consumption and archive all behavior tests.** After phases
   1-3, create a self-contained fixture with its own provider/service/factory,
   no KVStore2/example dependency and no private imports. Compile it both as a
   standalone consumer and as the workspace `downstream_test_util` integration
   binary. Its provider and bootstrap/write/read, restart, replacement,
   failover/isolation/quorum and switchover cases are ordinary nextest tests
   included in the reusable archive. A separate ignored Rust source gate
   packages the runtime/controller and compiles the same test bodies against
   their isolated package closure, with no workspace path/patch fallback or
   inherited repository environment. This separates real behavioral execution
   from feature/privacy/package compilation proof rather than hiding behavior
   behind a Python wrapper.
5. **Qualify failure and ownership behavior.** Extend phases 3-4 with factory
   errors, invalid provider state, startup cancellation, callback/worker errors
   and panics, deadlines, cleanup failure, early Drop, stale/cross-cluster
   references, unsupported transport, entered fault tokens and multiple
   concurrent clusters. Check actual listener/task completion and same-storage
   reopen, not merely emptied handle registries. Negative compile fixtures
   continue to reject production host/authority internals, including from a
   consumer with controller `test-util` enabled. Transport tests must reject
   a second manual/armed cut without changing the first, cover pending/entered
   Build cancellation, and keep the slot occupied until drop-triggered
   restoration is acknowledged. Delayed old-owner restoration cannot affect
   a new cut; a failed gate acknowledgement is an explicit cleanup failure.
6. **Qualify features, distribution and documentation.** After prior acceptance
   gates, check controller compilation with no default features, explicit
   `test-util`, and all features; retain a runtime-only no-default-features
   check. An external negative fixture must reject
   `kuberic_controller::test_util` without the feature, while the enabled
   workspace fixture executes the real journeys. Verify production entry points remain
   unchanged under feature unification. Add module rustdoc, a complete
   compilable downstream factory/test example, feature/prerequisite and
   compatibility guidance, and controller/runtime package checks. Update
   README and runtime testing wording only when those surfaces exist. Record
   the as-built behavior and verification in a local, ignored PAW `Docs.md`
   during implementation; do not treat this proposal as that implementation
   record.

Ordinary reusable harness and counter application/provider tests stay
cluster-free and archive-discoverable. All nextest test binaries, including
ignored source checks, are included in the archive. Implementation/source,
compiler, API/privacy and package checks execute in a separate checkout-backed
source job; doctests also execute there outside nextest. Keep exact
source/archive/partition accounting correct as tests move and are added. Retain
[`check_runtime_public_api.sh`](../../scripts/check_runtime_public_api.sh)
and its protected-interface/privacy checks. No full PostgreSQL suite is
required for this extraction; use the documented smoke selection if a later
adapter changes PostgreSQL behavior.

Acceptance requires real independent application journeys, an observed
application failure and acknowledged cleanup in the ordinary archived suite,
plus compilation of those same consumer bodies against independently packaged
crates with feature-on/off and privacy assertions. The compiler-only source
gate must not be described as packaged behavioral execution. Passing only the
existing KVStore2 journeys, hiding the harness behind `cfg(test)`, or compiling
an API sketch is insufficient. The harness must be present in the packaged
controller library when `test-util` is enabled, not only in workspace builds.

## Evidence Limits

This harness proves the selected controller/application journeys through real
RPCs, replication and provider files under one process. Restart is cooperative
shutdown/reconstruction, not a process kill or power loss. All replicas share
the address space, scheduler and OS failure domain.

It does not establish image startup, application HTTP serialization, Kubernetes
admission/RBAC/watch behavior, Service routing/DNS, real PVC provisioning,
arbitrary network partitions, crash durability, native SF parity, future V1
port activation or V2 transactional-replicator conformance. Keep the owned
KinD and provider-specific suites for their existing claims.
