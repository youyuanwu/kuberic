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
restart recovery, reconnect, Node/Pod-loss failover, Node replacement, persistence or upgrade
workflow.

## Primary application-crash failover

With the original operator and Node/control sessions alive, the FM can react to
matched native loss of a fully admitted Primary application. It drives shared
native reconfiguration with committed history and a fresh configuration epoch,
and waits for final quorum/configuration completion before publishing the new
Primary. During `reconfiguring`, the operator withdraws Service routing; on
completion it selects the new Primary's exact original Pod UID and records
`status.primaryNodeId`. It never rewrites the frozen allocation or restarts Pods.

This is unplanned application-process failover, not the planned handoff below or
Node/Pod/network-partition recovery. Further loss in the reduced cohort remains
blocked until native restoration finishes. The failed application can remain an
owned zombie until original-host retirement;
normal native deletion and validated physical cleanup are still required for
every original Node before finalizer release.

## Restoring redundancy

After failover the FM normally deletes the exact predecessor, then builds a
fresh replica on that same original Node. Shared fube Node/Hosting activates a
successor through the committed fresh Open's registration lookup after native
creation admission and positive old-host reaping/group/root cleanup. It retains
the original Node/RA, Pod UID and control channels and uses a fresh host/IPC gate.
Successful forced predecessor cleanup is recorded but does not veto restoration;
failed or incomplete cleanup still blocks acquisition.

The set reports `restoring` while the admitted Primary stays routed. Native
copy/build produces a Ready Idle; a subsequent same-Primary configuration
change restores the fixed 3/2 membership before `serving` resumes. Another
application failure is supported only after this completion. Deletion during
repair disables publication, finishes the accepted native restore and then
tears down the current replica incarnations and every owned application.

This follows SF's volatile drop/rebuild semantics, not persisted Reopen. Native
repair lineage and retired-host receipts remain in the original in-memory owner;
restarting the operator still cannot recover or adopt them.

## Secondary application-crash repair

A matched current Secondary loss in fully admitted Serving membership first
drives a same-Primary native configuration removing that exact failed replica.
The operator reports `restoring` and retains the same Primary endpoint and
original Pod UID selector. Native reduced-membership completion precedes normal
predecessor deletion, fresh copy/build and full 3/2 re-admission. Both membership
changes increment the configuration epoch without changing its Primary lineage
or data-loss epoch. Further loss during repair blocks instead of granting
promotion. Bounded replay history does not impose a lifetime repair limit;
out-of-window messages fail exact admission rather than enabling adoption.

## Qualification

Mock-backed runtime tests cover independent original owners, reserved
listeners, stale parent UIDs, restart refusal and cleanup evidence. Real
KinD bootstrap, early-deletion, application-crash failover and Secondary repair
journeys run against the deployed operator;
their driver has only read access to workload resources. The switchover
journey additionally grants patch access to `kubericsets/status` to submit the
same target-Primary request as an administrator. The separate
staggered-shutdown qualification retains explicit deterministic reconciliation
scheduling. See the live inventory with `cargo test --workspace -- --list`.

Real-process safety coverage exercises unavailable progress from a paused,
lagging Secondary, quorum loss at the direct live-Primary endpoint, and fresh
replica failure during held native copy/final admission. Refusal withdraws FM
publication without claiming remote writer fencing or successful native
deletion. Physical cleanup of these failed fixtures is verified separately.
KinD additionally holds actual fresh-host copy and verifies non-voting Idle
state and unchanged Primary routing before releasing normal repair.

## Planned switchover

The API follows CNPG's target/current status pattern rather than adding a spec
request or annotation. In stable Serving membership, `status.primaryNodeId`
and `status.targetPrimaryNodeId` identify the admitted Primary. Read the
KubericSet and choose another node ID from `status.allocation.nodes`:

```sh
kubectl --context CONTEXT -n NAMESPACE get kubericset NAME -o json
kubectl --context CONTEXT -n NAMESPACE patch kubericset NAME \
  --subresource=status --type=merge \
  -p '{"metadata":{"uid":"CURRENT_UID","resourceVersion":"CURRENT_RESOURCE_VERSION"},"status":{"targetPrimaryNodeId":"TARGET_NODE_ID"}}'
```

Use the UID/resourceVersion from that observation; on conflict, reobserve and
revalidate the intended target. The caller needs patch access to the
`kubericsets/status` subresource. A target already equal to the current Primary
does not start a handoff. Do not edit the allocation, bindings, completion
receipt or current Primary fields.
Wait for `serving`, matching target/current Primary and an observed completed
receipt before submitting another target. `configurationEpoch` is the latest
native-completed membership; the receipt's `previousConfigurationEpoch` and
`configurationEpoch` identify the accepted handoff, not restart authority.

The operator first disables the client Service and clears endpoint publication,
then asks its original FM to retain the handoff. It reports `switching` with
the last native-completed `primaryNodeId`. Native phase-0 demotion revokes the
old writer; routing withdrawal alone is not writer fencing. Fube owns peer
continuation, actual progress/quorum/history selection, catch-up and every
Secondary activation. Only the exact successful final native completion from
the target authorizes changed Primary routing and `status.switchover.completed`.
Endpoints may change only as required by the new roles; replica and original
Node/Pod/application identities remain unchanged. Exact retries reuse the
accepted operation and do not reset its timeout.

Healthy handoffs can repeat/reverse from completed 3/2 membership, including
after Primary or Secondary application-crash repair. Each new intent freezes
the current completed epoch and replica incarnations. Exact command replay is
keyed to that baseline, not just the target node; stale commands cannot transfer
newer membership. A new accepted receipt replaces the prior completed one.

Application loss after completed handoff uses normal native failover or
same-Primary Secondary removal, then fresh copy/build and 3/2 readmission on the
same original Pod. The latest handoff receipt remains historical while passive
target/current fields follow the admitted Primary; it cannot cause automatic
failback. All original Node/RA/Pod owners and surviving applications are retained.

Application loss during handoff/repair and cancellation/retargeting before
accepted completion is observed remain blocked. Handoff completion with
down/dropped members is not admitted by Kuberic; there is no spare-node policy.
There is
no automatic abort, failback, restart recovery or generation-change overlap.
Deletion during an accepted handoff waits for native completion with routing
disabled, then performs normal final-membership deletion and positive cleanup.
Recreating a controller facade over the same live backend can continue; status
cannot admit a new FM/operator owner.

Scripted tests cover exact retries, early endpoint reports, invalid/missing
completions, stale status/effects, refusal and deletion during handoff.
Real-process qualification exercises either Secondary candidate, each lost
application role after completed handoff, positive predecessor cleanup, fresh
repair and successive transfers using current identities. KinD composes
reverse/repeated target-status requests with post-handoff Primary and Secondary
crash repair, deployed routing, old-writer rejection, original Pod lifetimes and
finalizer cleanup.
