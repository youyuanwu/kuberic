# Experimental namespace operator

`kuberic-operator` deploys the current fube-backed reconciler. It watches
KubericSets and their Pods, ConfigMaps and Services in one namespace, and
requeues even without Kubernetes events so native FM progress and deadlines
continue to be observed. Each KubericSet UID has its own original in-memory
FM/process admission and a reserved concrete Pod-IP listener. Name reuse cannot
reuse another UID's authority.

The crate's reusable library exports `KubericSet`, `evaluate`, `Reconciler`,
native FM ownership and cleanup contracts. Planning, reconciliation, runtime
and supervision remain separate modules. Its binaries are `kuberic-operator`
for the watch/requeue loop and `kuberic-node` for owned-node supervision;
deterministic tests call the library without starting the operator runtime.

This is not a production-ready or highly available operator. Install only one
instance per namespace. The generated Deployment has one replica and uses
`Recreate`, not rolling replacement or leader failover. Control traffic remains
unauthenticated and unencrypted; use a trusted isolated network that permits
direct traffic between workload and operator Pod IPs, including dynamically
allocated FM TCP ports.

## Build and install

Requires Linux, Docker, the repository toolchain and initialized submodules.
Build native Linux binaries compatible with Debian trixie:

```sh
git submodule update --init --recursive
cargo build --locked -p kuberic-operator --bins
cargo build --locked --manifest-path fube/Cargo.toml -p fube-node-runner --bin fube-node
mkdir -p target/operator-image
cp target/debug/kuberic-operator target/debug/kuberic-node target/operator-image/
cp fube/target/debug/fube-node target/operator-image/fube-node-real
docker build -f kuberic-operator/deploy/Dockerfile \
  -t REGISTRY/kuberic-operator:TAG target/operator-image
docker push REGISTRY/kuberic-operator:TAG
```

The runtime image contains the operator, `kuberic-node` owned-child supervisor,
and real fube runner. It does not contain an application package. Build a
workload image derived from it that installs your package at the KubericSet's
`spec.packagePath`. The sample used by KinD is assembled automatically by
`just kind-test`.

Choose an existing namespace and explicit Kubernetes context:

```sh
cargo run --locked -p kuberic-operator --bin kuberic-operator -- crd \
  | kubectl --context CONTEXT apply --server-side -f -
cargo run --locked -p kuberic-operator --bin kuberic-operator -- manifests \
  --namespace NAMESPACE --image REGISTRY/kuberic-operator:TAG \
  | kubectl --context CONTEXT apply --server-side -f -
```

The generated installation contains a ServiceAccount, namespace Role and
RoleBinding, and Deployment. CRD installation is a separate administrator
operation; the operator has no cluster-wide RBAC. Set the example KubericSet's
namespace and workload image before applying it. Pods receive their own UID
from the downward API and do not receive a Kubernetes API token.

`run` reads `POD_NAMESPACE` and `POD_IP` from the generated Deployment. Its
flags also expose the namespace, advertised IPv4 address, node control port,
teardown budget and requeue interval. The image is not published by this
repository.

## Cleanup evidence

The production-path `kuberic-node` supervisor, installed as
`/opt/fube/fube-node`, owns `/opt/fube/fube-node-real`. It validates its
downward-API Pod UID against the immutable runner configuration, forwards
SIGTERM immediately to the owned PID, and writes a bounded termination
message only after successful node exit, application reaping, owned group
cleanup and native resource/channel joins. Forced or failed cleanup is not a
successful receipt. Reports identify the parent UID, deployment, node and Pod
UID.

The operator attaches `operator.kuberic.io/cleanup-report` before publishing
node configuration and retains each Pod until its actual container termination
message has been validated and cached by the original backend. It removes only
its own report finalizer. Missing, foreign, restarted or failed evidence blocks
the parent, withdraws routing and retains the parent teardown finalizer.
Pod absence, annotations, channel loss and native deletion acknowledgements
cannot manufacture physical cleanup evidence.

Final `lifecycle_complete` JSON events include the exact original cohort,
validated reports, native deletion/FM joining and owned resource absence.
They are diagnostic output, not restart-recoverable authority.

## Restart and shutdown contract

Native ownership and cleanup receipts are intentionally in memory. A new
operator process **never adopts an existing allocation**, even if its status
and termination messages look complete. It marks that KubericSet blocked,
removes client publication and retains cleanup finalizers. A new process may
allocate a genuinely fresh KubericSet; it cannot recover an old one.

SIGTERM/SIGINT stops reconciliation and attempts to invalidate locally owned
FM intent, disable Services and persist blocked status. It does not initiate
deletion of every deployment or claim that remote writers stopped. Abrupt
process/node loss can leave routing enabled until a replacement operator
successfully observes and withdraws it. Neither API reachability nor immediate
withdrawal is guaranteed during failure.

Delete and positively clean all KubericSets before intentionally replacing or
uninstalling a healthy operator. After unsupported owner loss, intervention
must establish actual original-node/application shutdown before removing
retained finalizers or deploying a fresh lifetime. There is no automatic
restart recovery, reconnect, failover, replacement, persistence or upgrade
workflow.

## Qualification

Mock-backed runtime tests cover independent original owners, reserved
listeners, stale parent UIDs, restart refusal and cleanup evidence. Real
KinD bootstrap and early-deletion journeys run against the deployed operator;
their driver has only read access to workload resources. The separate
staggered-shutdown qualification retains explicit deterministic reconciliation
scheduling. See the live inventory with `cargo test --workspace -- --list`.
