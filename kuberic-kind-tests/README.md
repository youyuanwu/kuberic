# KinD lifecycle tests

This crate runs the current fube-backed controller against real Kubernetes,
following the archived setup's dedicated cluster, explicit kubeconfig/context
and ownership-receipt contract. It does not deploy the archived operator.

## Run

Requires Linux, the repository Rust toolchain, initialized fube submodule,
Docker, KinD v0.30.0, kubectl and just. The native host binaries are packaged
into a Debian runtime image; use a native Linux Rust target.

```sh
git submodule update --init --recursive
export KIND_CLUSTER_NAME="kuberic-fube-$(date +%s)"
export KUBE_CONTEXT="kind-$KIND_CLUSTER_NAME"
export KUBECONFIG="$PWD/target/kind/$KIND_CLUSTER_NAME.kubeconfig"
just create-kind-cluster
just kind-test
just delete-kind-cluster
```

`kind-test` installs the generated current CRD, builds fresh binaries and loads
the image once per test executable. No manual application prebuild is required.
Nested Cargo builds use `target/kind-fixtures`, separate from the outer test
runner's lock. Only staged executables and sample manifests enter Docker's
build context. Each scenario gets its own namespace, service account, Role,
RoleBinding and non-restarting driver Job. Successful namespaces are deleted.
The default kubeconfig, existing clusters and mismatched ownership receipts
are rejected; Kubernetes commands always specify kubeconfig and context.

Real-cluster tests are explicitly ignored in ordinary workspace runs because
they require that owned cluster and Docker. They fail rather than skip if
explicitly selected without prerequisites. Unit checks still run normally.
See the live inventory with:

```sh
cargo test --locked -p kuberic-kind-tests -- --list
```

For failures, namespaces are retained and diagnostics include workload state,
events and logs. Inspect before deleting the cluster:

```sh
just kind-diagnostics
```

`just kind-images` and `just kind-install` expose image preparation and CRD
installation separately for debugging. CI runs the real-cluster journeys and
always attempts owned-cluster deletion.

## What is exercised

The host test process only prepares and observes the isolated cluster. A Job
runs the namespace-bound `Reconciler` and its original in-memory `ProcessFm`
inside Kubernetes, advertising the Job's concrete Pod IP. Workload Pods run
the actual fube runner and `hosting-local-app` stateful/data-replicator package.
Native FM/RA and replica links use concrete Pod IPs, not translated host ports
or Service VIPs. Only application traffic uses the primary ClusterIP Service.
The archived control-plane/worker layout is retained; the shorter kubelet sync
period keeps initial ConfigMap projection skew within the runner startup budget.

The healthy journey checks native final-configuration publication, exact primary
selection, a quorum-acknowledged write and readback through the Service, then
controller-owned deletion. The early-deletion journey requests deletion after
initial node configuration publication but before native bootstrap completes,
requires client routing to remain disabled, finishes the accepted bootstrap and
reuses the same teardown.

The staggered-shutdown journey also checks application write/read, then pauses
reconciliation after the first original Pod deletion. It requires that node's
validated physical-cleanup receipt while the other original Pods remain running
with their exact UIDs and no deletion requests. Only then does reconciliation
resume and delete the remaining Pods. This proves an original node can finish
before the others receive kubelet termination, rather than relying on signal
timing or simultaneous shutdown.

The image installs a test-only node wrapper at `/opt/fube/fube-node`, with the
real runner at `/opt/fube/fube-node-real`. The wrapper owns the actual node child,
forwards kubelet SIGTERM, checks successful exit, reaping, no surviving original
application PID, final runner cleanup and resource joins, and rejects forced
cleanup. It writes that exact deployment/node/Pod-UID report to the container
termination message. A test-only Pod finalizer retains the message until the
backend validates and caches it; only then is that finalizer released.
Missing reports, replacement UIDs, killed containers or incomplete cleanup
fail explicitly. Pod absence and native deletion replies never fabricate receipts.
Each wrapper forwards kubelet SIGTERM immediately to its own original runner;
there is no cross-node shutdown barrier. The pinned fube runner keeps lost-peer
control uncertainty separate from successful owned local shutdown. Neither
successful local cleanup nor connection loss substitutes for native deletion.
The driver separately requires native deletion and FM joining, checks owned
resources disappeared, and the host checks disappearance again before success.

This is a test harness, not a production controller/watch-loop installation.
It retains original backend authority only for the lifetime of the Job. It
does not add failover, persistence, reconnect, restart adoption or upgrades.
Control traffic remains unauthenticated and unencrypted; use only the isolated,
trusted test cluster.
