# Kuberic

Kuberic is being rebuilt on top of [fube](fube/). The initial implementation is
`kuberic-fm`, a process-only external FM for fube's experimental
`external-fm-volatile-three-v1` profile.

## Current scope

The FM owns fresh bootstrap, initial primary placement, sequential replica
builds, native 3/2 admission, endpoint publication, exact retries and normal
replica deletion. Its deterministic reconciliation core and native-channel
adapter use the same fube codecs as the real nodes.

Only one fresh volatile partition with three immutable original nodes is
supported. A Kubernetes-facing bootstrap library is qualified against a mock
API and local processes; real-cluster deployment is not supported. There is no
replica persistence/recovery, failover, switchover, replacement, reconnect,
resize or rolling upgrade. FM state is in-memory; restarting the FM does not
recover the deployment. A lost original
channel or invalid completion produces explicit control uncertainty and withdraws
FM endpoint publication. It does **not** prove that an existing writer stopped.
Control TCP is unauthenticated and unencrypted; use a trusted, isolated network.

## Build and basic tests

```sh
git submodule update --init --recursive
cargo build -p kuberic-fm
cargo test --workspace
cargo clippy --workspace --all-targets --no-deps -- -D warnings
```

The `kuberic-tests` crate owns the integration suites and their shared support:
FM decisions with scripted native replies, real loopback channels, and Kubernetes
reconciliation through a mock-backed `kube::Client`.
Shared helpers live in `kuberic-tests/src` and are imported through the crate.
On Linux, it also builds and launches the real FM, node and sample
application binaries automatically. Its process journeys verify application
write/readback and lifecycle cleanup, not failover or recovery.
Use `cargo test --workspace -- --list` for the live inventory.

## Kubernetes bootstrap controller

`kuberic-controller` provides a namespace-bound `Reconciler`, a pure evaluator,
the `operator.kuberic.io/v1alpha2` `KubericSet`, and a `ProcessFm` adapter around
the existing native FM. It is a library, not a deployed watch loop. Callers must
reconcile repeatedly, including stable/waiting states; each call applies a
bounded effect or returns an explicit blocked/waiting outcome. The example
[KubericSet](kuberic-controller/examples/kubericset.json) describes the sample
package, not an available image or installation.

Bootstrap proceeds through separately observed facts:

- Install the owned-teardown finalizer and persist fresh allocation authority
  before creating owned objects.
- Create empty per-node ConfigMaps, a disabled client Service and all original
  Pods. Pods use `restartPolicy: Never`; their startup gate waits for `node.json`.
- Observe all Pod IPs and bind exact Pod/ConfigMap UIDs before freezing topology.
  Pod Running/Ready is not native replica readiness.
- Start the original native FM listener, then publish immutable node
  configurations. `{node_ip}` in parameter values expands to that node's frozen
  IP; target/minimum sizes remain 3/2.
- Publish the opaque native endpoint and select the exact original primary Pod
  only after final native configuration completion.

Owned-resource writes retain UID/resourceVersion checks and effects revalidate
their observation. A `StaleObservation` error means the planned effect was
refused; callers reobserve and retry. Lost API replies do not roll back
persisted authority.
Changed or missing original Pods/configurations, container restarts, unsupported
spec changes, lost allocation status or unavailable original backend admission
block the cohort. Blocking invalidates further locally owned FM intent and restores a
nonmatching Service selector; it does not prove a remote writer stopped.
Foreign-owned resources are not mutated.

The initial volatile profile creates no PVCs. The desired client port is an
application contract and must match its listener; opaque native endpoint
strings are preserved, not reinterpreted as a universal Kubernetes port format.
The Pod template requires an image containing `tini`, a POSIX shell, `sleep`,
the fube runner and the declared application package. No such image, RBAC,
installation, real-cluster garbage-collection or Kubernetes networking
qualification is provided yet.

Tests use a narrow in-memory HTTP API behind a real `kube::Client`, including
namespaces, ownership, UID/resourceVersion fencing, immutable ConfigMaps and
explicit conflict/lost-response injection. A mock-Pod supervisor supplies only
infrastructure facts and launches local fube processes from the published
configurations. Native bootstrap/build/configuration still uses the real FM.
Controller-facade recreation can reuse retained backend admission; a fresh
backend cannot adopt API status, reconnect or reconstruct process cleanup.

### Persisted authority versus backend state

The CRD stores Kubernetes/native intent, not local supervisor internals:

| Persisted fields | Purpose |
|---|---|
| Parent namespace/name/UID, accepted generation and spec | Exact Kubernetes ownership and frozen desired intent |
| Deployment/partition, native FM identity/instance/generation/address, node/replica IDs | Original native protocol authority |
| Pod/ConfigMap names and UIDs, Pod IPs and control ports | Exact object incarnations and native listener bindings |
| Phase, native endpoint, reason, bootstrap-wait start/deadline, teardown cohort/Service UID/stage/start/deadline | Observed progress and retained operation scope |

`fmOwner`, `processOwner`, runtime `Settings`, host run directories and raw
process-reaping receipts are not CRD fields. `Settings` is a controller input;
native FM authority and concrete control ports are captured explicitly rather
than persisting the whole runtime configuration.
Older experimental status shapes are rejected, not converted into backend
admission or cleanup evidence.

FM/process backends privately reserve a fresh allocation before its first
status write. No resources or native effects start until that write is observed.
Failed or lost writes reuse the same retained draft. For an existing allocation,
the controller requires original backend admission; it never calls `reserve`
to import API status. Admission permits acquiring that fresh cohort, but is not
itself evidence that any process was joined.

`packagePath` is the package's in-container path, and node configurations use
the Pod's `/run/fube` volume. The local process supervisor translates these to
host fixture paths only in its private launch configuration; those paths are not
written into Kubernetes objects.

### Controller-owned deletion

The controller retains `operator.kuberic.io/owned-teardown` until the original
cohort is positively cleaned up. Deletion before allocation removes the finalizer
without starting anything, provided no owned resources or native session exist.
After allocation, deletion finishes the accepted bootstrap before normal teardown.
The controller continues provisioning and native creation for the original
allocation, keeps client routing disabled, and never publishes Serving status or
a client endpoint while deletion is pending. Later spec edits cannot retarget
this bootstrap. It does not cancel individual native build steps.

Once native creation completes, deletion drives these separately observed stages:

- Persist teardown intent for the exact frozen cohort and original Service UID,
  with a bounded deadline. Later spec edits cannot retarget that accepted intent.
- Restore the nonmatching client Service selector, then request native FM
  shutdown. Wait for actual deletion replies for every original replica.
- Request normal UID/resourceVersion-fenced Pod deletion with a positive grace
  period. The mock supervisor forwards SIGTERM to its owned node processes.
- Wait independently for original node/application reaping and resource joins,
  Pod disappearance, and actual FM task joining. A Pod deletion, native reply,
  signal or closed channel is not a physical-cleanup receipt.
- Delete the exact original ConfigMaps and Service, persist completion, then
  remove only this controller's finalizer. Foreign finalizers are preserved.

`Reconciler::new` requires an `FmApi` and a separate `ProcessApi`. The process
backend must retain exact cohort/Pod-UID receipts from actual owned child
handles, including successful node exit, application reaping, resource joins
and whether cleanup was forced. The local process tests publish those facts
only after checking the real fube exits, final cleanup status and owned joins.
Kubernetes annotations and object absence cannot substitute for that backend.

Reconciliation may resume after lost API replies or controller-facade recreation
with the same retained backend admission. Creating fresh backends is not recovery.
Cleanup receipts remain backend observations, not CRD data; persisted teardown
progress cannot substitute for them. Every resource-release/finalizer step still
requires live original cleanup evidence. API conflicts do not advance cleanup.
Missing, failed, forced, partial or changed evidence retains the finalizer and
surfaces uncertainty instead of successful teardown.

`Settings::teardown_timeout_ms` separately bounds the bootstrap-completion wait
(when needed) and subsequent teardown. Each operation persists its start/deadline
before effects; retries, controller-facade recreation and later runtime policy
changes cannot extend an accepted deadline. Bootstrap failure or timeout retains
the finalizer and requires intervention rather than attempting an unsafe abort.
Unhealthy cohorts, premature Pod loss, changed original resource UIDs, or an
expired cleanup deadline also remain explicitly blocked. There is no
forced-deletion or automatic adoption path. The finalizer workflow is qualified
only against the mock API and local processes, not a real-cluster garbage
collector or remote process backend.

## Process mode

```sh
cargo run -p kuberic-fm -- --config kuberic-fm/examples/fm.json
```

Start the FM first, then all three `fube-node` processes without waiting for one
to become service-ready. Prepare their packages/configurations according to
[fube's process guide](fube/docs/cluster/ExternalFmContainerNode.md). Match the
FM identity/generation, deployment, partition, original placements and package
description in the FM configuration. The example uses the `hosting-local-app`
package, selects node 2 as primary, and requires node parameters overriding
target/minimum sizes to 3/2.

Follow fube's concrete, immutable IPv4 address rules. Application listeners may
use dynamic ports when the application supports them; preserve the complete
endpoints returned by native replies. The FM emits JSON status events on stdout
and publishes the primary endpoints only after matched final native completion.
Status is diagnostic, not an application access grant or a health probe.

For healthy shutdown, stop client traffic and send SIGTERM/SIGINT to the FM.
It withdraws publication and drives normal, non-force deletion. Wait for
`awaiting_node_shutdown`, then signal all original node runners. Keep the FM
running until their original channels close. Its `stopped` status means native
deletion was acknowledged and original channels were joined, **not** proof of
physical node/application cleanup; inspect node exits and final cleanup status
separately. Early shutdown before create completes fails explicitly and requires
external node teardown. Before a fresh deployment, verify every old node and
application stopped and use fresh lifetime identities/run directories.

### Real-process tests

On Linux, the process tier runs as part of normal workspace tests. To run it alone:

```sh
cargo test --locked -p kuberic-tests --test processes
```

The binary-dependent journeys share one test executable, separate from the mock
and native-channel suites. A shared `OnceLock` builds `fube-node`,
`hosting-local-app` and `kuberic-fm` before their first use. Cargo checks freshness
on each test run;
build failures fail the tests rather than skipping them. Nested builds use the
separate `target/process-fixtures` directory to avoid the outer test build's lock.

The process tests exercise the standalone FM binary and the mock-backed
controller/native-FM bridge with real node binaries and sample application
children. They check write/readback, native-controlled client routing and
controller-driven finalizer teardown with independent process-cleanup receipts.
Deletion during node startup also qualifies bootstrap completion without client
publication, followed by the same native deletion and physical cleanup.
They need no Docker, Kubernetes or native Service Fabric.
`FUBE_NODE_EXE`, `FUBE_NODE_APP` and `KUBERIC_FM_EXE` override individual
executables; overridden binaries are validated and are not built by the helper.

The fixture follows fube's temporary bind-to-port-zero allocation for node
control addresses. Releasing those sockets before child startup leaves a port
allocation race; inherited/pre-bound listeners are not supported yet.

## Previous implementation

The previous implementation is preserved in [`archive/`](archive/), including
its source, documentation, deployment assets, build configuration, and CI.
Archived workflows are outside `.github/workflows/` and are inactive.
