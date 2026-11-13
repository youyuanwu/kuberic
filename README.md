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
cargo fmt -p kuberic-fm -p kuberic-controller -- --check
cargo clippy --workspace --all-targets --no-deps -- -D warnings
```

Default tests exercise FM decisions with scripted native replies, real loopback
channels, and Kubernetes reconciliation through a mock-backed `kube::Client`.
They do not launch node/application processes or prove replication correctness.
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

- Persist fresh allocation authority before creating owned objects.
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
their observation. Lost API replies do not roll back persisted authority.
Changed or missing original Pods/configurations, container restarts, unsupported
spec changes, lost allocation status or a different live FM owner block the
cohort. Blocking invalidates further locally owned FM intent and restores a
nonmatching Service selector; it does not prove a remote writer stopped.
Foreign-owned resources are not mutated.

The initial volatile profile creates no PVCs. The desired client port is an
application contract and must match its listener; opaque native endpoint
strings are preserved, not reinterpreted as a universal Kubernetes port format.
The Pod template requires an image containing `tini`, a POSIX shell, `sleep`,
the fube runner and the declared application package. No such image, RBAC,
installation, finalizer/garbage-collection workflow or Kubernetes networking
qualification is provided yet.

Tests use a narrow in-memory HTTP API behind a real `kube::Client`, including
namespaces, ownership, UID/resourceVersion fencing, immutable ConfigMaps and
explicit conflict/lost-response injection. A mock-Pod supervisor supplies only
infrastructure facts and launches local fube processes from the published
configurations. Native bootstrap/build/configuration still uses the real FM.
Controller-facade recreation can reuse the same retained live FM owner; a new
FM process cannot adopt status or reconnect to the old cohort.

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

### Real-process smoke test

On Linux, build the pinned fube binaries, then explicitly run the process tier:

```sh
cargo build --locked --manifest-path fube/Cargo.toml \
  -p fube-node-runner -p hosting-local-app --bins --target-dir target
cargo test --workspace -- --include-ignored
```

The process tiers exercise the standalone FM binary and the mock-backed
controller/native-FM bridge with real node binaries and sample application
children. They check write/readback, native-controlled client routing and owned
teardown. They need no Docker, Kubernetes or native Service Fabric.
`FUBE_NODE_EXE` and `FUBE_NODE_APP` can override the prebuilt binary paths.

The fixture follows fube's temporary bind-to-port-zero allocation for node
control addresses. Releasing those sockets before child startup leaves a port
allocation race; inherited/pre-bound listeners are not supported yet.

## Previous implementation

The previous implementation is preserved in [`archive/`](archive/), including
its source, documentation, deployment assets, build configuration, and CI.
Archived workflows are outside `.github/workflows/` and are inactive.
