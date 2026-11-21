# Restart-safe operator and FM ownership

**Status: proposed, not implemented.** This addresses GAP-01 in the
[feature-parity tracker](feature-parity.md). Current deployments remain subject
to the [existing restart contract](../kuberic-operator/README.md#restart-and-shutdown-contract).
This document adds no recovery capability and requests no immediate runtime
qualification work.

## Decision summary

Persist the minimum non-reconstructible FM authority on the existing
KubericSet. Drive progress through Kubernetes watches, native observation events
and requeues. Re-observe resources and native owners on every reconciliation;
do not checkpoint the FM process or introduce a general operation/event journal.

Use CNPG's target/current intent and observation-driven controller pattern,
while retaining SF's exact configuration epochs, membership and native
completion barriers. A separate FM-state CRD is not the default design.
Persist new authority before issuing its effects, not every intermediate
observation or callback.

Follow ordinary SF FM's persisted-partition-state and retry model. Do not copy
its storage engine, implement a second replicated database, or treat every
operator restart as an empty-store FM rebuild.

Use one pure FM decision core behind the Kubernetes reconciler, with separate
native and Kubernetes effect adapters. Do not wrap the current autonomous
`ProcessFm` in persistence callbacks and retain a second decision loop.

The target remains one volatile partition, fixed target/minimum 3/2, and the
same original Pods and Node/RA/RAP/Hosting/application owners. Operator-only
restart does not inherently require persistent application storage: surviving
applications retain their volatile data. Losing those owners is a different
failure.

## 1. Design basis

The chosen approach combines CNPG-style target/current convergence with SF's
native configuration authority and completion barriers. Kubernetes provides
metadata durability; native replica quorum remains independent.

Detailed evidence and approaches not selected are maintained separately:

- [Service Fabric FM storage and recovery](background/operator-restart-research.md#service-fabric-fm-state-and-recovery).
- [CNPG intent and observation-driven reconciliation](background/operator-restart-research.md#cnpg-compact-intent-and-reconstructed-progress).
- [Current Kuberic implementation evidence](background/operator-restart-research.md#current-kuberic-implementation-evidence)
  and [alternatives not selected](background/operator-restart-research.md#alternatives-not-selected).

## 2. Recovery problem

The existing KubericSet is the place for minimum authority, but current
observed status, backend ownership and native transport do not yet provide a
restart contract. Recovery needs accepted intent before effects and repeatable
native observation/completion, not a checkpoint of controller memory.

Example: native handoff A-to-B completes, but the operator crashes before its
status projection records B. A replacement must neither republish A nor
bootstrap a new partition. It needs the accepted A-to-B configuration and
native completion reconciliation.

The answer is a small explicit authority contract plus repeatable observation
and native recovery, not a larger dump of diagnostic status.

## 3. Minimum durable authority

### 3.1 Storage placement

Keep canonical authority on the namespace-scoped KubericSet, reusing its
accepted allocation/bindings and adding a small versioned native-authority
section where needed. The object's UID is the lifetime/store identity;
`resourceVersion` is the CAS token. Do not add a separate `KubericFmState`,
event log or generic operation-resource family by default.

Existing phase, endpoint, `primaryNodeId`, target request and handoff receipt
fields remain the user-facing interface. A request is not an accepted native
configuration: the controller validates it against current authority and
atomically accepts a configuration intent before issuing native work.

All authoritative updates use the same parent-object CAS boundary. Diagnostic
status refresh must preserve canonical authority and pending intent; it must
not overwrite them from a stale observation. Only write status when the
meaningful value changes, not for each poll or native retry.

Status remains controller-owned. Direct administrator target patches are
trusted administrative operations, as with the existing API. Kubernetes RBAC
cannot protect individual status fields: do not claim that granting status
write permission grants access only to the target field. If untrusted
delegated promotion access is later required, design field validation or a
separate request interface then; do not introduce a second authority store
preemptively.

### 3.2 Retain a fact only if it cannot be safely reconstructed

The [persisted-state contract](operator-restart-state.md) defines proposed
field names/types, ownership, commit points, reconstruction/removal rules and
a complete disposition of the current FM `State` fields. It is not implemented.
The table below summarizes its semantic minimum; avoid duplicating fields
already retained by allocation or exact-UID-bound immutable configuration.

| Group | Minimum durable facts and reason |
|---|---|
| Lifetime | Existing accepted deployment/partition, logical FM identity, original Node/Pod bindings and schema version; names alone cannot authorize adoption |
| Native owner fence | The committed owner token/term and negotiated native generation/instance, needed to supersede stale control sessions |
| Completed configuration C | Exact native-completed epoch and current membership/incarnations; a Primary name is not a membership proof |
| Accepted configuration/replica intent T, when different | One frozen semantic successor: kind, PC/CC epochs, target membership or fresh replica allocation, and its baseline; needed because native effects may outlive the controller |
| Non-derivable fences/bounds | Incarnation-allocation high-water or per-transition redirection budget only when not implied by retained C/T; restart must not permit identity reuse or reset a safety bound |
| Irrecoverable boundary evidence | Compact native/physical-cleanup receipts only where acknowledgement or resource deletion would remove the last reconstructible evidence |

The concrete contract reuses the existing epoch/Primary fields as part of C
for version-1 lifetimes. It derives roles, PC membership, replica allocation
high-water and redirection counts instead of adding independent copies.
Following native FM pending-action behavior, retry clocks and per-attempt
timeouts remain local. There is no persisted command deadline or timer-created
terminal hold. A late Kubernetes write does not invalidate a still-current,
proven native completion; see the
[source-grounded retry policy](operator-restart-state.md#31-match-native-fm-pending-action-behavior).

`C == T` (or no T) describes an admitted stable configuration. `C != T`
describes accepted work still requiring observation/native completion. Repair
can also retain a fresh replica intent before it becomes voting membership.
This is the SF equivalent of CNPG's target/current separation, not a reduction
of an SF failover unit to two node names.

Once T is accepted, a later target request cannot abandon it or cause a second
epoch allocation. Finish or explicitly block that native transition first,
then evaluate any subsequent request against the newly completed C. Retain the
existing epoch-fenced handoff semantics rather than copying CNPG's ability to
reset an absent target.

T stores normalized semantic fields, not a serialized outbound message. Derive
the native body from C/T plus exact immutable configuration and allowed fresh
native observations. Freeze any protocol-significant field that cannot be
re-derived unchanged. Refreshing an endpoint must not silently alter an
already accepted command.

Keep the following out of authoritative persisted state:

- Full native reports/inventories, LSN samples and copied RA phase state.
- Every callback result, per-Node upload ACK, retry count, send timestamp or
  "next workflow step" cursor.
- Wire envelopes, raw message bodies and durable copies of in-memory
  completion/candidate-report replay deques.
- Resource counters, Pod readiness/IP snapshots, conditions and derived phase
  used as substitutes for live observations. Diagnostic copies may exist,
  but they are not recovery authority.
- Sockets, task/child handles, PIDs, local admission caches and relative clocks.

This minimization depends on an explicit fube observation/replay contract.
If a needed fact cannot be re-queried/replayed after a new session, it is not
reconstructible. Either add that native capability or retain the smallest
semantic receipt before acknowledging/discarding the fact. Never omit required
evidence merely to claim a smaller schema.

Existing exact-replay caches may remain bounded in memory. Across a restart,
derive outcomes or stale rejection from C/T and native replay; persist a
dedicated replay key/result only if a concrete protocol obligation cannot be
met that way. No unbounded historical-operation or inventory collection is
part of this design.

### 3.3 Initialization, deletion and missing authority

Install the existing parent finalizer, then CAS accepted lifetime intent before
native effects. Reconcile deterministically named support resources from the
parent UID. Observe and freeze the FM discovery Service/Pod bindings before
publishing native configuration. An incomplete allocation may converge support
resources but cannot start native replicas.

A lost status-write response is resolved by reading the same KubericSet UID;
it must not create a second deployment, partition or replica allocation.
Changed accepted spec or inconsistent bindings block.

Keep the parent and its authority through native and physical cleanup.
Persist a compact receipt before releasing any child retention that would
erase its last evidence. Remove the parent finalizer only after the existing
positive lifecycle contract is satisfied; no second store-object finalizer
is needed.

If established authority is missing, corrupt or incompatible, block. Do not
rebuild it from `primaryNodeId`, endpoint strings, Pod readiness or resource
names. Parent recreation, etcd data loss/rollback and recovery from backup
require a separate lost-authority design; restarting a process is not
authorization to run SF-style empty-store rebuild.
The supported failure model assumes Kubernetes has retained committed writes.
It does not promise detection of every coordinated metadata rollback.

## 4. Watch-driven reconciliation and commit-before-effect

### 4.1 The control loop owns progress

Use the existing Kubernetes reconciler as the high-level FM driver. Watches
enqueue a KubericSet key; they do not carry indispensable workflow state.
After restart, initial list/watch synchronization queues existing sets even
when no new user edit occurs.

Observe the KubericSet, exact owned resources and original native owners, then
evaluate the next bounded convergence action:

```text
KubericSet/child watch, native event, initial list, or timer
    -> enqueue exact set UID/key
    -> observe desired intent + Kubernetes resources + native reality
    -> validate AuthorityView and derive the permitted execution mode
    -> evaluate FM policy and resource/lifecycle predicates
    -> CAS new authority only if a decision/completion must be retained
    -> ensure the already-accepted native/resource effect
    -> wait for observation change or requeue deadline
```

In addition to KubericSet, Pod, ConfigMap and Service watches, bridge native
report/completion/control-loss notifications into the same queue. Kubernetes
watches cannot observe an RA transition that does not change an API object.
Use bounded requeues for native retries, quiet in-flight work and deadlines.
Watch delivery is a latency optimization, not the sole correctness mechanism.

The transport adapter can own sockets and native retry/receive plumbing.
Fube RA/RAP/Hosting continue their own native work. Neither adapter becomes a
second autonomous high-level controller that selects new FM authority outside
the KubericSet reconcile/CAS path.

Coalesce queue notifications only when their facts remain re-observable or
replayable. Preserve correlated native request ownership/backpressure; do not
drop an unrecoverable report because another queue notification already exists.
No native callback requires a status patch unless it changes C/T, an essential
fence, or irrecoverable boundary evidence.

### 4.2 Commit decisions, not a workflow transcript

For a new authority decision, use an explicit commit gate:

```text
observation + committed C/T
    -> validated next C/T or essential receipt
    -> compare-and-swap KubericSet status
    -> issue effects dependent on that accepted decision
```

Effects already justified by committed intent need no new "sent" write before
every retry. This applies to bootstrap, loss handling, candidate redirection,
repair allocation, handoff and deletion, not just a restart method.

Rules:

1. Persist exact intent before sending a command that depends on it.
2. A prepared command means "may have been sent". A successful TCP write is
   not a native completion and does not resolve a crash window.
3. Validate native evidence against committed intent. Commit a completed
   configuration, changed accepted intent or essential receipt before effects
   depending on it. Re-queryable progress stays in memory. An ACK that would
   destroy the last needed evidence requires the compact durable consequence
   first; this is not a requirement to journal every ACK.
4. On an ambiguous Kubernetes write, stop dependent effects and read the
   canonical object directly. Determine whether the exact transition committed;
   never allocate a new epoch/operation simply because the reply was lost.
5. On conflict, discard the uncommitted proposal and re-evaluate. Never merge
   two conflicting authority proposals field by field.
6. Retry or regenerate an already committed operation only while its native
   state/result policy permits it, without changing semantic identity. Fresh
   owner terms/generations are transport authority, not reasons to mutate PC/CC
   or restart application Open.

An accepted C/T pair is a compact semantic plan, not an ordered queue of
messages or a general saga. Derive which native step is needed from current
observations. If a distinction genuinely cannot be reconstructed, name that
specific semantic boundary in the schema rather than adding a generic
per-step checkpoint.

Persist accepted intent and required outcome facts, not a controller retry
transcript. Use local retry pacing and bounded I/O/reconcile attempts; stalled
health does not revoke T. A proven successful completion can be folded after
an API outage without proving it happened before a controller deadline.
Native failures/timeouts retain their action-specific handling. Clock changes,
expiry and unavailable API access cannot manufacture completion, declare a
replica dead or authorize data loss.

### 4.3 One decision core, explicit effect adapters

The recovery-enabled Kubernetes path replaces the autonomous `ProcessFm`
facade with the following ownership boundaries:

| Component | Responsibility and restriction |
|---|---|
| Read-only observers | Return exact Kubernetes/native evidence with UID, incarnation and correlation context; no status writes, finalizer changes or hidden resource effects |
| Validated `AuthorityView` | One in-memory interpretation of the committed allocation/bindings, owner, C/T, handoff, repair and teardown; reject contradictions before planning |
| Pure FM decision core | Own native FM configuration policy, epoch/replica allocation and evidence validation; propose decisions without I/O or internal authority advancement |
| Kubernetes reconciler | Derive resource/lifecycle work, invoke FM policy, perform the sole authority CAS and dispatch only effects justified by committed state |
| Effect adapters | Execute fenced Kubernetes mutations or native transport/negotiation/retry work; never independently select a target, allocate an epoch/replica or advance C |

The FM core returns an authority proposal, an eligible effect justified by
existing authority, or an explicit wait/block result. A proposal is not
accepted state: the reconciler must commit it and revalidate ownership before
dependent execution. Lost/conflicting CAS results follow section 4.2.
This applies to native report ACKs and candidate redirection as well as
outbound configuration commands.

Keep FM policy in the FM core, not a second copy in Kubernetes-specific
planning or inside RA. RA still owns history/quorum analysis; RA/RAP/Hosting
retain their native execution. A standalone process driver may wrap the same
core, but the Kubernetes adapter must not start another autonomous policy task.

`AuthorityView` is validated once per observed parent revision and rebuilt
after a changed/ambiguous write; it is neither stored nor a new cache of
authority. It validates cross-field invariants and distinguishes permitted
incomplete provisioning from corrupt established state. It does not certify
current native readiness. Diagnostic code receives a derived projection,
not an unrestricted authority-mutating interface.

Resource effects include installing report retention, releasing it after a
committed receipt, publishing configuration and updating routing. Observation
and pre-effect revalidation cannot mutate these resources. Backend-local
`reserve`/allocation/cohort caches are not recovery authorization; validate
against the committed view and exact live resources instead. Any local cache
is disposable and must not require the old process to have admitted the set.

## 5. Control ownership and stable native transport

### 5.1 Elect a controller, then establish native authority

A namespace Lease can coordinate operator leadership. Claiming ownership of
each set must additionally CAS a new owner token/term into its canonical
KubericSet status. All later authority writes require the exact parent UID,
resourceVersion and owner token. Lease acquisition alone grants no native
configuration authority.

Derive and persist native FM instance/generation advancement through the
[explicit mapping in the state contract](operator-restart-state.md#23-field-meanings-commit-points-and-lifetime),
which uses native FM instance as the owner term for this profile. Native
generation, configuration epoch and data-loss epoch remain separate domains;
their integer encodings are not implicitly interchangeable.

The new owner must establish the native generation/instance fence on the
required original Nodes before issuing replica/configuration effects. First
bounded support requires all three original Nodes to participate. This is a
conservative controller-recovery admission condition, not a new RA quorum rule. It applies
to serving/unfinished native work, not the receipt-complete
[cleanup-only path](#cleanup-only-recovery). During incomplete bootstrap,
finish permitted resource/configuration provisioning before requiring every
original Node to be reachable; see the derived modes in section 6.

An old controller can have commands already in flight. They refer only to
previously committed intent; reconcile their real results. After handover,
stale-generation/instance traffic must be rejected. A former owner must not
negotiate higher generations from private memory or reuse a new owner's
connection. This requires explicit native session ownership, not merely a
Lease check followed by an unguarded send.

Takeover must also work while the old FM TCP connection remains open. The
new owner needs an admitted handover path to the known original Node control
addresses, or another explicit native discovery/handover mechanism. It must
not wait solely for the old connection to fail or for a Node to happen to
reconnect. Repointing an EndpointSlice affects new connection routing, not
established sockets, native generation or FM ownership. R1 remains gated until
this path and old-session disposal are defined by fube.

The required live-owner handover sequence is:

1. Claim the KubericSet owner instance and commit the proposed native
   generation before any message depending on it.
2. Reach each required original Node at its frozen control address over a
   candidate connection even while the old session is live. Fube must admit
   only native negotiation for the configured logical FM and authorized new
   incarnation; ordinary configuration traffic is not yet allowed.
3. Use native generation proposal/update semantics. Validate exact Node
   identities and generation responses; re-read canonical ownership before
   any further proposal or generation increase. Unexplained higher authority
   blocks rather than rewinding a Node or reconstructing a lost FM store.
4. Bind the new session only through that negotiated fence. Old session
   traffic must become stale without waiting for its TCP connection to close;
   drain/cancel its transport contexts without discarding native work.
5. Process each required Node's nonempty generation/configuration inventory
   on the new session, committing any essential consequences before ACK.
   Keep the per-round readiness set in memory and repeat discovery if this
   process dies. Upload ACK advances its native generation obligation, not
   replica admission or writer authority.
6. Only after the required fences and observations succeed may the new owner
   resume eligible effects for committed C/T. Obtain real final completion
   separately; do not reopen applications to make the new socket usable.

This specifies R1/R2 obligations, not a new private takeover message or an
implemented transport path. The current native codecs alone do not provide
candidate-session admission. It is an adaptation for the external-FM profile,
not SF empty-store rebuild: C/T remains authoritative throughout.

Persist the negotiated round before relying on its effects. If Nodes report a
higher admitted generation, re-read the canonical record and reconcile it with
retained negotiations before proposing another round; never rewind a Node.
Unexplained authority or configuration history blocks rather than overwriting
accepted C/T from a report. Generation upload completion remains separate from
final replica reconfiguration.

### 5.2 Give the lifetime a stable FM endpoint

For newly recovery-enabled lifetimes, advertise a per-KubericSet selectorless
ClusterIP Service rather than the current operator Pod IP. Retain its
UID/address for that lifetime. Give its single TCP port the name `fm-control`;
all per-set Services may use the same Service port because their IPs differ.

Use one deterministically named, controller-managed EndpointSlice per Service.
Bind it to the exact Service UID and mark its owning KubericSet UID/term. Its
single endpoint is the active operator Pod IP, and its `fm-control` port is
that owner's reserved concrete listener port. Different sets can therefore
use different local ports without changing their advertised Service addresses.
The deployment/RBAC must permit namespace-scoped EndpointSlice management and
traffic to those concrete listener ports.

After committing ownership, bind and initialize the listener for negotiation,
then CAS the EndpointSlice to that owner/endpoint. Apply the projection fencing
rules below; never append a second ready slice during takeover or assume that
closing an old listener alone fences native traffic. Foreign or conflicting
slice ownership blocks rather than publishing mixed backends.

Listener readiness means it can perform recovery negotiation, not that the
partition can serve clients. The Service routes connections; exact native
identity/generation checks establish authority. This does not depend on
rebinding a previous Pod's IP, changing original Node addresses or restarting
their peer mesh.

Fube must treat the configured endpoint as a discovery/bootstrap location and
support admission of the authorized new FM instance. It cannot keep requiring
the first configured remote process incarnation forever.

### 5.3 Reconnect without replacing native owners

On FM disconnection, the Node must retain its Node/RA/RAP/Hosting and application
owners while it attempts the supported control reconnection. A new connection
replaces only transport/session ownership after validation.

Handle multiple connection attempts, old queued traffic and cancellation with
bounded ownership. Do not start a second Node, re-run application Open, reset
native epoch history or grant a role to make the new socket usable. Peer-channel
replacement and whole-Node restart remain separate work.

Existing replicas may still serve according to native partition authority and
actual quorum. Neither FM disconnect nor Service withdrawal proves writer
shutdown. This design makes no zero-downtime guarantee during recovery.

## 6. Recovery is normal reconciliation after ownership handover

Derive the permitted execution mode from validated authority, receipts and
current prerequisite observations. Do not persist a mode/stage field:

| Mode | Selection and permitted work |
|---|---|
| Provisioning prerequisites | Accepted initial allocation still needs permitted original resource creation, binding or configuration publication; converge those prerequisites under Kubernetes ownership without requiring all native owners first |
| Native control required | Native work remains, including bootstrap with no C; establish the required native admission before replica/configuration effects |
| Cleanup-only | Accepted teardown has no T/repair and native deletion receipts cover frozen C; perform only fenced physical/Kubernetes cleanup, without native construction or admission |

Cleanup-only takes precedence over resource convergence: never recreate its
retired resources. Provisioning is a prerequisite path, not proof that no
native work has happened. In particular, absent C with bootstrap T may mean
commands were already executed. Re-enter native observation/replay after
prerequisites converge rather than constructing fresh replica state.

1. **Load.** Directly read and validate KubericSet authority and UID,
   original bindings, schema and pending lifecycle obligations into
   `AuthorityView`. Watch caches are notifications, not takeover authority.
   Classify the mode before constructing a native adapter/listener.
2. **Claim.** Acquire leadership, CAS a new owner term and establish the
   required Kubernetes child-mutation fences. Cleanup-only continues through
   section 7 instead of the native path below.
3. **Converge prerequisites.** Complete permitted initial support resources
   and freeze exact bindings, including the discovery Service UID/address.
   Once required bindings are complete, initialize the negotiation listener
   and fenced EndpointSlice, then ensure each original Node's immutable
   configuration is published. A crash between individual publications must
   not require every Node to exist before finishing the missing publications.
   Do not replace a missing already-bound Pod/Node, rewrite a published
   configuration for the new owner, or infer fresh replica state from absence.
   Deletion retains its existing bounded acceptance/teardown rules.
4. **Admit and observe original owners.** Complete the required per-Node native
   ownership/generation handshake. Nodes may negotiate as they become
   reachable; replica/configuration effects still require all three.
   Collect incarnation-bound inventory and native pending/completed-operation
   evidence from the surviving Nodes.
   Keep re-queryable inventory in memory; persist only required changes to
   C/T, ownership fences or otherwise irrecoverable evidence.
   Startup `NodeUp`/`ReplicaUp`, reconnection and LFUM generation upload are
   distinct paths; do not manufacture one by replaying fresh empty bootstrap.
5. **Reconcile.** Match inventory to the committed FT/configuration and
   accepted operation. A matching completed configuration can restore stable
   knowledge. An interrupted operation resumes its exact intent using native
   retry/completion replay. Changed identities, contradictory epochs, missing
   required owners or unsupported interruption stages block.
6. **Confirm completion.** Obtain the actual final native completion required
   by the operation. Inventory/upload success alone cannot finish copy,
   catch-up, RAP work, deletion or a role change. Where completion was lost,
   retry the same supported command to recover its native receipt.
7. **Commit only changed authority.** If the accepted configuration completed,
   fold T into C and clear its pending intent atomically. Otherwise retain T
   unchanged and keep converging. Refresh derived status as needed. Activate
   the reconstructed adapter for that committed owner, not a fresh allocation.
8. **Project and continue.** Reconcile public status and client routing from
   the committed, currently validated result, then accept subsequent work.
   Deletion intent takes precedence over publishing a recovered endpoint.

Do not promote during recovery merely because status or one Node names a
Primary. RA still performs GetLSN, PC/CC quorum and history analysis whenever
native reconfiguration requires them. FM authorizes configuration/epoch
changes; recovery does not transfer RA's algorithm into the operator.

### Kubernetes projection fencing

Canonical authority and public status share a KubericSet CAS boundary; writes
to Services/EndpointSlices are still separate. Stamp mutable child projections
with parent UID, owner term and configuration/publication identity. Reuse an
accepted configuration epoch as that identity where sufficient instead of
adding another progress counter. Treat resourceVersions as opaque CAS tokens.

Takeover must establish a new-term CAS barrier on mutable projection objects.
A stale worker may only use observations bound to its own term and expected
UID/resourceVersion; it cannot re-read a newer object and stamp old intent
with the newer term. After the new-term barrier, queued old-version writes
must conflict and old-term workers must refuse further mutation.

Before that barrier, the old projection can remain visible; API outages can
also delay withdrawal. Therefore stale routing must never be treated as
native writer authority. A successful old intent or lagging public status is
reconciled, not interpreted as permission to undo a completed handoff.

Updating an FM-discovery Service and publishing the client Primary are separate
effects with separate readiness predicates.

## 7. Restart during teardown

Keep the existing distinction between native deletion and physical cleanup.
The Node supervisor still owns/reaps the actual child and produces the
incarnation-bound termination report; the replacement operator does not adopt
a PID or infer a receipt from Pod absence.

Replace the mutating `PodProcesses::observe` pattern with a read-only evidence
observer and explicit reconciler-owned retention effects:

```text
read-only observer validates original Pod termination report
    -> reconciler commits compact exact-UID receipt by parent CAS
    -> Kubernetes effect adapter removes report finalizer with child UID/RV fence
    -> later tolerate Pod absence using the durable receipt
```

Today the validated receipt is cached in memory before removing the Pod
finalizer. Without a durable intermediate step, a crash can lose the only
retained evidence after the Pod disappears.

Installing report retention is also an explicit fenced resource effect,
required before permitting Pod deletion. An observer cannot add a finalizer
as a side effect of a read or recreate missing retention after deletion has
begun. Missing required retention follows the existing visible failure path.
Neither observer nor effect adapter writes parent authority; this avoids
introducing a second status writer to repair the receipt ordering.

Native deletion acknowledgements and lifecycle targets must also survive
restart. The parent finalizer is removable only after durable exact native
deletion, positive physical cleanup and owned-resource absence. Losing the old
FM task handle is not an unjoinable eternal requirement: the new recovery
contract must reconstruct protocol closure obligations, while retaining the
same positive remote-cleanup requirement. It must not pretend it joined a
vanished local task.

Foreign, failed, forced or missing reports retain the existing failure
semantics. No annotation or administrator-edited public status can substitute
for an accepted supervisor report.

### Cleanup-only recovery

When accepted teardown has no T/repair and durable `nativeDeleted` covers
every member of frozen C, do not require those Nodes to reconnect. Some or
all original Pods may already have been correctly destroyed.

Claim/revalidate Kubernetes ownership through the parent CAS and retain the
exact child UID/resourceVersion and stale-owner projection guards. Then:

1. Keep client publication disabled and ensure original Pod shutdown/deletion.
2. Observe any remaining supervisor termination reports, commit each exact
   `cleanupConfirmed` receipt, then release that Pod's report retention.
   Pod absence without the matching receipt still blocks.
3. After all physical-cleanup receipts and Pod absence, delete the remaining
   exact owned support resources and finish parent finalization.

This branch does not start an FM, recreate discovery resources, negotiate a
generation, issue new native configuration or join the vanished FM task.
The stored native-generation floor can remain unchanged; a fresh Kubernetes
owner term is not a claim that a native FM session was admitted. Close/drain
only local adapters actually acquired by this process.

Complete native deletion authorizes Pod shutdown, not physical-cleanup
success. R5 must establish that explicit original Node/supervisor shutdown
can drain safely after FM transport loss without reconnecting or obtaining
new FM acknowledgements, including safe cancellation/drainage of transport
ownership. The exact positive supervisor report is still required. If that
composed closure cut is unsupported, leave it blocked rather than inventing
cleanup evidence.

Bound each cleanup attempt and surface actual failure, but elapsed controller
time cannot supply, expire or invalidate a real receipt. If native deletion is
incomplete, use the ordinary live-owner handover gate and retain the current
conservative refusal for unsupported missing-owner cuts. The branch is derived
from existing receipts; it adds no persisted cleanup stage.

## 8. Crash-window behavior

| Failure point | Required result |
|---|---|
| Initial authority/allocation write reply lost | Read the exact parent UID; reuse committed allocation or wait; no second lifetime |
| Initial allocation committed, discovery Service or resource binding unfinished | Resume permitted provisioning; atomically bind the observed Service UID/address before publishing original native configuration |
| Only some original Node configurations published; C still absent | Finish missing exact publications without requiring all Nodes first; then admit/observe native owners and reconcile retained bootstrap T, not reset native state |
| Intent committed, command not sent | Regenerate the same semantic command |
| Command completed, completion write absent | Reconcile/retry for native completion; do not allocate another epoch or republish the old Primary |
| Exact current-T completion arrives after a controller/API timeout | Validate and fold it under current ownership; no time-created terminal hold |
| Completion committed, public status/Service stale | Repair projections from canonical state |
| Authority-store write result unknown | Resolve by direct read before dependent effects |
| Old operator resumes after takeover | Store CAS and native session fences reject stale authority; projection barriers reject stale writes |
| API server unavailable or Lease renewal lost | Stop new authority/effects as ownership becomes uncertain; no claim that surviving writers stopped |
| Required live native-control owner missing before deletion completes | Block bounded operator-only recovery; do not turn it into Pod-loss failover |
| Native deletion complete; original Nodes/Pods already retired | Resume cleanup-only from durable receipts; do not require native handover or recreate FM discovery |
| Cleanup receipt committed, Pod already gone | Continue using that exact durable receipt |
| Pod gone without a committed cleanup receipt | Block finalization |
| Parent/established authority absent, recreated or known/suspected rollback | Fail closed; lost-store rebuild and rollback recovery are outside this design |

## 9. Required upstream fube contracts

The pinned fube baseline is
`d8a56a99a2b3c6f38875577af9737413f6568e89`. It already has live-session
generation/LFUM machinery, but its
[documented scope](../fube/docs/cluster/ExternalFmContainerNode.md#ordinary-fm-lfum-status)
explicitly does not make that startup recovery or production FM rebuild.
Its [limits](../fube/docs/cluster/ExternalFmContainerNode.md#failure-behavior-and-limits)
still reject reconnect and FM restart.

This baseline includes the local cluster's extracted
[`fube-fm` process](../fube/crates/tools/fube-cluster/README.md#fm-process-boundary).
Its private management IPC separates FM intent/native Channels from Node
process supervision, but the FM remains in-memory and tied to same-host launch
metadata/receipts. It is not the externally listening FM used by Kuberic and
does not establish the cross-session recovery contracts below. The extraction
does not replace this design's pure decision core and Kubernetes CAS boundary.

Native control now uses the shared `furpc` session adapter; the former
`fube-transport` crate is retired. Kuberic follows that transport API without
changing its FM authority or adding reconnect/restart support.

Before Kuberic may enable recovery:

- Support FM control disconnect/reconnect while retaining original native
  owners, including safe disposal of old Channel contexts.
- Define authorized FM identity/instance/generation takeover and stable
  endpoint discovery, not arbitrary peer adoption.
- Provide nonempty discovery/reconciliation for supported live replicas and
  interrupted work. Preserve SF's distinction between initial replica upload
  and LFUM generation upload, including their separate acknowledgements.
- Preserve or reconstruct exact pending native work and replay final
  completions across the new control session. Inventory snapshots alone are
  insufficient for effects whose completion is still pending.
- Establish explicit post-deletion Node/supervisor closure after FM transport
  loss, without new FM handshakes or fabricated completion/cleanup evidence.
- Expose enough authoritative lifecycle/configuration observations or
  completion replay to derive progress from C/T. Do not require the operator
  to save RA phase cursors, full inventory snapshots or every callback. Name
  any non-replayable boundary that still needs a compact durable receipt.
- Preserve semantic message identities while resealing transport/session
  headers; reject old-generation and wrong-incarnation control traffic.
- Make unsupported recovery states explicit without changing native quorum,
  promoting from upload ACKs, restarting application owners or granting
  persisted Reopen to volatile replicas.

These are protocol/lifecycle capabilities, not changes to the protected
`Replicator`, `PrimaryReplicator` or `StateProvider` method sets. A Kubernetes
store adapter cannot supply them on fube's behalf.

## 10. Delivery boundaries

Recommended increments:

1. **Minimal authority and convergence:** add only the missing C/T, owner and
   non-derivable fence/receipt fields to KubericSet. Extract the pure FM
   decision core and validated `AuthorityView`; replace the autonomous
   Kubernetes `ProcessFm` path rather than adding persistence callbacks.
   Make watch/native-event/requeue reconciliation derive execution modes and
   progress through a single-object CAS gate, with read-only observers and
   explicit fenced effects. No separate FM-state CRD or general journal.
   Keep current restart refusal until native recovery exists.
2. **Upstream native handover:** FM-only reconnect and fenced ownership with
   all original Nodes alive. Add the supported fube contract before enabling
   it in Kuberic.
3. **Stable-cohort restart:** recover an idle, fully admitted 3/2 set with no
   accepted in-flight operation; preserve identities/data and permit later
   ordinary work. An accepted transition or changed owner still blocks.
4. **Interrupted lifecycle recovery:** enable explicitly supported cuts during
   bootstrap, failover/redirection, handoff, repair and teardown, retaining exact
   intent and completion barriers. The receipt-derived cleanup-only branch
   does not need native handover, but still requires safe post-deletion closure
   and positive supervisor receipts. Only then claim full restart parity for
   those journeys.

Future qualification should cover the crash windows above, stale former
owners, lost store/native replies, data continuity and positive cleanup.
These are acceptance criteria for later implementation, not instructions to
run new suites now. Genuine unequal-history candidate-selection qualification
remains separately deferred and is not a dependency of this design.

New recovery-enabled lifetimes must start with the durable schema and native
protocol. Existing lifetimes created under the current frozen Pod-IP/fresh-only
contract are not retroactively adopted. No old-state conversion, automatic
operator image upgrade, whole-Pod/Node restart, membership resize, application
spare pool, destructive data-loss policy or etcd disaster recovery is included.
