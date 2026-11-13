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
supported. There is no Kubernetes integration, persistence/recovery, failover,
switchover, replacement, reconnect, resize or rolling upgrade. State is
in-memory; restarting the FM does not recover the deployment. A lost original
channel or invalid completion produces explicit control uncertainty and withdraws
FM endpoint publication. It does **not** prove that an existing writer stopped.
Control TCP is unauthenticated and unencrypted; use a trusted, isolated network.

## Build and basic tests

```sh
git submodule update --init --recursive
cargo build -p kuberic-fm
cargo test --workspace
cargo fmt -p kuberic-fm -- --check
cargo clippy --workspace --all-targets --no-deps -- -D warnings
```

Default tests exercise FM decisions with scripted native replies and real
loopback channels. They do not launch node/application processes or prove
replication correctness. Use `cargo test --workspace -- --list` for the live
inventory.

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
cargo test -p kuberic-fm --test processes -- --ignored
```

This launches the actual FM binary, node binaries and sample application
children, checks write/readback and verifies owned teardown. It needs no Docker,
Kubernetes or native Service Fabric. `FUBE_NODE_EXE` and `FUBE_NODE_APP` can
override the prebuilt binary paths.

The fixture follows fube's temporary bind-to-port-zero allocation for node
control addresses. Releasing those sockets before child startup leaves a port
allocation race; inherited/pre-bound listeners are not supported yet.

## Previous implementation

The previous implementation is preserved in [`archive/`](archive/), including
its source, documentation, deployment assets, build configuration, and CI.
Archived workflows are outside `.github/workflows/` and are inactive.
