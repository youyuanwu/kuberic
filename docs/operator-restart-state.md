# Minimal persisted FM state contract

**Status: proposed, not implemented.** This specifies the storage contract for
the [operator restart design](operator-restart-design.md). It does not update
the CRD or enable restart recovery. Native observation/replay dependencies
below are requirements, not capabilities claimed for the current fube pin.

The contract covers the existing fixed 3/2 volatile, original-Node profile.
New lifetimes use this schema from bootstrap. Existing deployments are not
silently imported. External-system research and unselected approaches remain
in the [background document](background/operator-restart-research.md).

## 1. Authority and ownership rules

Persist accepted decisions that cannot be reconstructed, not a workflow
transcript. Reconciliation derives the next effect from accepted intent,
owned Kubernetes objects and actual native observations.

All authority lives on one KubericSet UID and changes through its status CAS.
The leader's authority writer is the sole writer of the fields below.
Diagnostic updates preserve them; administrative target requests do not
overwrite an already accepted transition.

Three classes of value have different meanings:

| Class | Meaning |
|---|---|
| Accepted authority | Identity, configuration and outstanding intent committed before dependent native effects |
| Boundary receipt | A compact validated fact retained before acknowledging/removing its last source |
| Observation | Re-queryable native/Kubernetes state; may be cached or projected, but cannot independently authorize an epoch or writer |

Never infer success from a missing field, absent Pod, expired timer, completed
upload or successful socket write.

Decode and cross-validate one parent revision into an in-memory
`AuthorityView` before planning. It checks the combined C/T, handoff, repair,
teardown and lifetime invariants below, including permitted incomplete
provisioning. It is not another stored record or proof of native readiness.
Rebuild it after a changed/ambiguous write. The
[component boundaries](operator-restart-design.md#43-one-decision-core-explicit-effect-adapters)
keep FM decisions pure, parent CAS in the reconciler and observation read-only.

## 2. Concrete status layout

The names and types below define the proposed version-1 contract, not executable
Rust definitions. Optional means absent, not a permissive default for malformed
or partially written authority. Closed records reject unknown fields.

### 2.1 Reuse existing fields

| Existing location | Version-1 treatment |
|---|---|
| `metadata.uid` | Lifetime/store identity; no duplicate store UUID |
| `status.allocation` | Retain accepted deployment, partition, service/spec, logical FM identity, original Node IDs and initial replica assignments |
| `status.bindings` | Retain exact original Pod/ConfigMap UIDs and topology; immutable after binding |
| `status.configurationEpoch` | Canonical epoch of the last native-completed configuration C |
| `status.primaryNodeId` | Canonical Primary of C; retain it during reconfiguration even while client routing is withdrawn |
| `status.targetPrimaryNodeId` | User-facing requested target, not accepted configuration authority |
| `status.switchover` | Reuse the latest typed handoff receipt for accepted source/target/epochs and completion; retain only the latest receipt |
| `status.bootstrapWait` | Existing timing may remain diagnostic; expiry does not cancel accepted bootstrap or certify cleanup |
| `status.teardown` | Retain accepted deletion scope/client Service UID and compact receipts; existing timing is diagnostic, not completion authority |
| `status.phase`, `reason`, `serviceEndpoint` | Derived presentation, not substitutes for C or native completion |

The epoch/Primary fields currently project in-memory FM status. For
recovery-enabled version 1 they become part of C and must be committed with
its membership. In particular, do not clear `primaryNodeId` merely because
routing is disabled. This is a deliberate schema/semantics change for new
lifetimes, not current implemented behavior.

Add `status.allocation.fm.serviceUid` when binding the stable FM-discovery
Service. Existing `allocation.fm.address` becomes that Service's lifetime
address. In version 1, the UID/address pair is absent while the initial
Service is unbound; this is a permitted incomplete provisioning state, not an
empty string, placeholder address or partially valid native endpoint. After
observing the exact owned Service, commit its UID and assigned address
together before publishing any original native configuration. Once bound,
both are immutable; a missing/replaced bound Service blocks rather than
authorizing a new binding. This deliberately changes the current required
address field for new versioned lifetimes; it is not implemented.
The allocated logical FM ID remains unchanged.
`allocation.fm.instance`/`generation` describe initial bootstrap seeds; current
owner values below must not rewrite immutable original runner configurations.
Accepting a later authorized FM incarnation is an upstream requirement.

Other support-resource names and the EndpointSlice/listener backend are
reconciled from parent ownership and current owner observations. Do not copy
their mutable addresses, ports or resourceVersions into another durable cache.

### 2.2 New authority section

```text
NativeAuthorityV1 {
    schemaVersion: 1
    owner: Owner
    members: Map<NodeId, ReplicaIdentity>
    accepted?: AcceptedIntent
    repair?: RepairIntent
}

Owner {
    holderId: UUID
    instance: u64
    generation: i64
}

ReplicaIdentity {
    replicaId: i64
    replicaInstance: i64
}

AcceptedIntent =
    { kind: "bootstrap" }
  | { kind: "switchover" }
  | { kind: "loss", configurationEpoch: i64, candidateNodeId?: NodeId }
  | { kind: "admitRepair", configurationEpoch: i64 }

RepairIntent {
    nodeId: NodeId
    predecessor: {
        replicaId: i64
        replicaInstance: i64
        configurationEpoch: i64
    }
    successor?: ReplicaIdentity
    predecessorDeleted: bool
}
```

The section is stored at `status.nativeAuthority`. An authority-bearing
partial record is invalid; initial allocation/binding may be incomplete only
as explicitly permitted by section 2.1 and the initial resource-binding gate.

Create the complete initial authority section in the first accepted-allocation
CAS: C absent, `members` empty, bootstrap intent present and the initial owner
bound. Complete exact resource binding before opening the native effect gate.
An older allocation without this version marker is not a recoverable new
lifetime.

Derive provisioning, native-control and cleanup-only execution modes using the
[recovery ordering](operator-restart-design.md#6-recovery-is-normal-reconciliation-after-ownership-handover);
do not persist a mode field. Provision missing original configuration before
waiting for every Node to be reachable. C absent with bootstrap T does not
prove native effects never occurred, so it cannot by itself authorize a
fresh-state reset. A missing already-bound original owner remains a blocked cut,
not permission to replace it.

### 2.3 Field meanings, commit points and lifetime

| Field | Why stored / source | Commit before | Reconstruction / removal |
|---|---|---|---|
| `schemaVersion` | Pins validation and deterministic native-command recipes | First native effect | Immutable; incompatible versions block |
| `owner.holderId` | Identifies this controller process, not merely its Pod | Native ownership negotiation | New UUID per process; same-Pod restart is a new holder |
| `owner.instance` | Monotonic native FM instance and explicit Kuberic owner term | Advertising/admitting the new owner | CAS increment on takeover; never reset or reuse |
| `owner.generation` | Native ordinary-FM generation candidate/admission floor | Sending a proposal/update relying on that value | Reconcile negotiation without rollback; no per-Node ACK bitmap |
| `members` | Exact active replica IDs/instances, including completed repairs | Publication or work depending on completed C | Change only with validated final native configuration; preserve until superseded by completed C |
| `accepted` | The one accepted successor T which may already have native effects | First dependent command/ACK | Keep unchanged on retry; clear only with its validated completion or a specifically supported superseding acceptance |
| `repair` | Identifies the failed predecessor after it leaves C, plus its one allocated successor | Loss-report ACK, predecessor deletion or successor creation, as applicable | Retain until C contains the admitted successor; no lifetime retired-replica list |
| `predecessorDeleted` | Exact normal-deletion receipt which may disappear when the native host is replaced | Successor creation | Monotonic false-to-true; bound to the retained predecessor; clear only with the whole finished repair |

For this profile, `owner.instance` deliberately doubles as the Kuberic owner
term; do not persist a second equal counter. The native generation remains
independent. This mapping requires fube to support authorized instance
takeover; it does not follow merely from similar integer types.

There is no persisted controller retry clock or catch-all error latch.
Connection loss, an in-progress upload and an ordinary retry are re-observed
under the admitted recovery contract. Invalid authority and unsupported native
outcomes still block explicitly without changing C/T; diagnostic error text
remains in `status.reason`/logs. If a necessary terminal fact cannot be
re-observed, retain its smallest semantic receipt before discarding its source
or keep that recovery cut unsupported. Do not silently forget the fact.

### 2.4 Completed configuration C

C is the atomic combination of:

```text
status.configurationEpoch
status.primaryNodeId
status.nativeAuthority.members
```

Before first admission, the first two are absent and `members` is empty.
After admission, both exist; membership contains two or three distinct
original Nodes and includes the Primary. Replica identities are valid and
unique. Every C member was Ready/up in the validated final completion; current
liveness is an observation, not a permanently true stored bit.

Roles are derived: the named Primary is P and other members are S. Do not
store another Primary or role array. Ready Idles are not C members.
Completed PC is empty; sizes are fixed 3/2. Node instances, service/package
version and partition description are reconstructed from exact immutable
allocation/configuration.

Version 1 derives the data-loss epoch `1` from this profile's existing native
initialization and rejects other histories. Persist the configuration-epoch
scalar as signed 64-bit, without lossy numeric conversion. Supporting another
data-loss epoch requires an explicitly extended contract, not a default value
in a partially decoded record.

Require positive owner instances/generations, nonnegative replica IDs,
positive replica instances, checked bounded integers and exactly one Primary.
C, handoff intent and target epochs must agree; a diagnostic writer cannot
repair contradictory authority by choosing one copy.

On unplanned Primary change, update the passive `targetPrimaryNodeId` to the
new admitted Primary in the same completion CAS. Do not retain the old passive
target as an automatic failback request. Requests cannot queue or retarget
accepted work outside the existing epoch-fenced handoff rules.

### 2.5 Accepted intent T and command derivation

T is semantic intent, not a serialized native request or generic step cursor.
There is at most one accepted intent. Its kinds have closed invariants:

| Kind | Retained inputs and deterministic interpretation |
|---|---|
| `bootstrap` | C absent; original assignments and initial Primary come from allocation order/profile. Existing native initialization gives configuration epoch `0x1_0000_0001`, then initial 3/2 admission uses the following same-Primary epoch. Native observations determine which create/build/admit effect remains |
| `switchover` | C is healthy 3/2 at acceptance. Existing `status.switchover` is an incomplete frozen receipt whose source/baseline match C; it supplies the target/new epoch without a duplicate copy in T |
| `loss` | C was fully admitted 3/2 and the exact native loss was validated. `repair.predecessor` identifies the failed member and original deletion epoch. If it was C's Primary, `candidateNodeId` is required; otherwise it is absent and C's Primary stays P |
| `admitRepair` | C has two members and `repair` names the third Node's fresh successor with `predecessorDeleted=true`. Acceptance requires actual successful Ready-Idle build evidence. T admits that identity as S without changing the Primary |

Loss T retains C's membership as PC; the failed member becomes down/None in
CC. Successor admission derives C plus the fresh Idle-to-Secondary member.
Handoff derives previous/current roles from C and the frozen handoff target.
All initial/live native descriptions must still satisfy existing strict
service, identity, state and endpoint validation.

Initial Primary-loss nomination follows the existing first-surviving-original
Node policy and is frozen in T. Only a validated native recommendation may
redirect it; a new observation/restart does not rerun candidate policy.

For primary-changing work, configuration epochs advance by
`PRIMARY_INCREMENT = 0x1_0000_0001`; same-Primary removal/admission advances
by one, with existing overflow and Primary-epoch checks.

No separate redirection counter is needed. During Primary loss:

```text
redirectCount =
    (T.configurationEpoch - C.configurationEpoch) / PRIMARY_INCREMENT - 1
```

Require a positive exact multiple, unchanged data-loss epoch, the same C
baseline, and the existing original-node-count bound. Persist each redirected
T before replying/issuing its command. Restart must not create another epoch
for an already accepted report.

Do not silently expand this to arbitrary supersession: accepted handoff,
repair, further application loss and deletion keep their existing bounded
composition rules. A later user target does not replace an in-flight T.

### 2.6 Repair allocation without a lifetime high-water field

Accepting loss records the predecessor before C drops it. On successful loss
reconfiguration, atomically commit the new C, clear loss T, and allocate the
one successor into `repair.successor` before any fresh native creation.

Use the existing allocation policy:

```text
next = 1 + max(all replica IDs and instances in
               original allocation, C, repair.predecessor, repair.successor)
successor.replicaId = successor.replicaInstance = next
```

Original initial replica instances are derived from the fixed profile.
Checked arithmetic is required. Once allocated, the successor cannot be
discarded/reallocated because a send or response was lost. After successful
admission it appears in C before `repair` is cleared in the same CAS.

Consequently, no previously allocated maximum falls out of the retained set
while another allocation is permitted. No separate high-water counter is
needed for the current no-abandonment, one-repair-at-a-time profile. Future
abort, resize or supersession which discards an allocated identity must first
add an explicit fence or prove another retention rule.

### 2.7 Native wire views are not synthesized progress

C/T does not contain endpoint strings, first/last LSN samples or copied RA
phases. Those are available only from an exact, incarnation/configuration-bound
native observation or supported command/completion replay.

This is a concrete upstream dependency, not permission to weaken validation.
Current same-Primary requests copy completed native replica descriptions, and
some completion checks compare them exactly. An arbitrary new inventory
snapshot is not necessarily the frozen description needed to retry that
command. Fube must expose/replay that stable view or establish a precise
SF-valid canonical reconstruction rule.

Unknown-LSN sentinels may be derived only where the existing native recipe
requires them, such as failover/handoff progress collection. Do not normalize
arbitrary LSNs/endpoints away to make the small schema appear sufficient.
Until that contract is provided, the affected restart cut remains unsupported;
do not claim C/T alone can reconstruct today's complete `Replica` objects.

## 3. Native retry semantics and deletion receipts

### 3.1 Match native FM pending-action behavior

Native SF FM regenerates eligible pending actions from committed partition/
replica state and rate-limits their dispatch. Its reconfiguration time limit
produces stuck health; it is not a blanket deadline after which a still-current
native completion must be rejected. See the
[local source findings](background/operator-restart-research.md#native-fm-retry-and-timeout-semantics).

Use that model here:

1. Re-observe C/T and native reality; regenerate only the eligible effect for
   the same accepted intent.
2. Keep retry pacing, I/O deadlines and reconcile execution budgets local.
   Restart may restart pacing, but cannot allocate a new replica/epoch or
   substitute a different target.
3. If one attempt times out or the API is unavailable, report the uncertainty
   and requeue. Do not record a permanent partition failure merely because
   a Kubernetes completion write was late.
4. Accept and CAS an exact successful completion for the still-current T
   whenever native ownership, identity, epoch and completion validation
   succeed. Elapsed controller time is neither success nor revocation of T.
   Superseded, foreign and stale completions remain fenced.
5. Continue retrying only while native state/result policy permits that
   action. Explicit native errors are not all transient: process their
   source-aligned transition where supported, otherwise report blocked with
   C/T and evidence retained. Do not turn a failed build into a successful
   Idle or keep retrying an unsupported failed lifecycle blindly.

A native failure policy which would abandon an allocated successor or change
membership outside the supported C/T transitions remains unsupported until
its identity-retention and native recovery rules are designed. Matching retry
semantics is not permission to import every SF repair/placement policy.

This deliberately replaces the current process-only FM's fatal session
timeouts for the new recovery-enabled profile; the existing runtime is not
changed by this document. Do not persist `commandWindow`, send counts or a
generic `DeadlineExceeded`/`ClockUncertain` hold as configuration authority.

Native RA/RAP progress, catch-up, build, close and runtime safety timers remain
their owners' responsibility. Preserve bounded RPC/process-cleanup attempts
and explicit failure reporting. A timed-out attempt may still have run;
reconcile its real result rather than overlapping another owner.

SF also has explicit retention, placement and configured quorum-loss/data-loss
policies. Those are not a generic retry deadline and are not added to this
fixed volatile profile. Controller timeout never waives quorum, supplies a
deletion receipt or grants data-loss authority.

Existing diagnostic timestamps can report long-running/stuck work. Accurate
health age is an observability concern, not permission to abort or accept a
configuration. Clock uncertainty stops decisions which require trusted time,
not validation of an otherwise proven native completion.

### 3.2 Extend existing teardown state, not a second cleanup store

Keep the accepted `serviceUid`. Existing `startedUnixMs`/`deadlineUnixMs`
may remain diagnostic for this profile, but are not fatal durable-fold
boundaries. Add only:

```text
TeardownReceipts {
    nativeDeleted: Set<NodeId>
    cleanupConfirmed: Set<PodUid>
}
```

These are fields on `status.teardown`, not another object. Missing sets in a
fully initialized version-1 teardown are invalid; empty means no receipts.

| Field | Commit rule | Retention and reconstruction |
|---|---|---|
| `nativeDeleted` | Add a Node only after its exact successful native deletion reply | C is frozen throughout native teardown, so the Node maps to one exact replica/epoch; no duplicate full reply body is needed |
| `cleanupConfirmed` | Add a Pod UID only after validating the original supervisor report's successful exit, reaping, joins and non-forced cleanup | Commit before removing the Pod cleanup-report finalizer; retain through parent finalization |

An element of `cleanupConfirmed` means the complete version-1 positive cleanup
predicate succeeded, not just process exit. Do not store a loose Boolean that
can mean different partial results. Failed/forced/missing reports never add
an element; retain their source and report the failure explicitly.

The deletion timestamp prevents new user work/publication immediately.
Deletion during bootstrap/handoff/repair waits for already accepted native
work under the bounded composition policy; elapsed controller time does not
cancel it or authorize cleanup. Accept `status.teardown` only once that policy
admits normal shutdown, with complete 3/2 C and no outstanding T/repair.
Freeze C and native deletion targets at that boundary; no later allocation
may change them.

Initialize both receipt sets atomically with teardown acceptance. All set
elements must match the frozen original scope; a deadline cannot add one.

`teardown.cohort` need not duplicate immutable top-level allocation/bindings in
this new contract. Native/physical/resource stages are derived from receipts
and exact resource observations, rather than trusted from a saved `stage`:

1. Pending native receipts: do not delete Pods.
2. All native receipts: ensure original Pod deletion; retain cleanup evidence.
3. All positive cleanup receipts and Pod absence: ensure support-resource
   deletion and current adapter closure.
4. All obligations complete: remove the parent finalizer.

Existing stage values may be projected for diagnostics. A stage name, lost
old task handle, socket close or Pod absence cannot substitute for a receipt.
Do not claim to join a vanished process's local task; reconstruct protocol
closure obligations under the recovered owner.

### 3.3 Cleanup-only admission after native deletion

Classify this case immediately after reading canonical authority, before
constructing or admitting a native FM. Complete `teardown.nativeDeleted`,
bound to frozen C with no T/repair, selects cleanup-only recovery even if
original Nodes are no longer reachable.

Acquire a new Kubernetes owner under the same CAS/child-mutation fences, but
do not negotiate native generations, recreate FM/discovery resources or
require an old FM stopped/joined observation. Continue UID-fenced Pod shutdown,
positive report capture and support-resource deletion. A new owner instance
used as a Kubernetes fence need not have an admitted native session; leave
the native-generation floor unchanged when no native work is issued.

If physical reports are still pending, R5 must allow explicit original
Node/supervisor shutdown to drain safely despite FM transport loss, without
new FM acknowledgements. Persist each validated receipt before releasing its
source. `nativeDeleted` alone never supplies `cleanupConfirmed`, and an absent
Pod without its durable physical receipt still blocks.

Keep attempts bounded and failures visible, but do not expire receipts or
require manual intervention solely because the controller was down past a
diagnostic deadline. Incomplete native deletion still requires ordinary
native-control recovery and its conservative live-owner gate. No new persisted
branch/stage is needed.

## 4. Atomic updates and crash-window rules

Only the following semantic boundaries need authority writes. Repeated polls,
upload notifications and native retries do not each create a persisted step.

| Boundary | One status CAS must retain | Effects allowed afterward / restart behavior |
|---|---|---|
| Fresh allocation | Accepted lifetime, versioned initial owner and bootstrap intent; discovery UID/address pair may be absent | Converge permitted original resources only; lost write reply reuses the same UID/allocation |
| Exact resource binding | Original Pod/ConfigMap bindings and the discovery Service UID/address, with its pair committed together | Once all required bindings exist, ensure original native configuration publication; partial publication is not proof of fresh native state |
| Owner takeover | Fresh holder/instance; proposed generation only where native recovery is needed | Receipt-complete teardown takes cleanup-only; otherwise negotiate/connect before new configuration |
| Native command attempt/retry | No new progress write; the accepted C/T already justifies it | Rate-limit locally; a completed send is not a native result |
| Initial admission | C epoch/Primary/members and removal of bootstrap intent | Publish only after the actual final 3/2 completion and live validation |
| Handoff acceptance | Frozen `switchover` receipt and `accepted.kind=switchover` | Withdraw routing, then send the one accepted native configuration |
| Loss acceptance | Exact predecessor anchor, loss T and the selected candidate if needed | ACK the validated loss; withdraw routing for Primary loss and drive native reconfiguration |
| Candidate redirection | New loss T epoch/candidate for the same C, preserving repair anchor | Reply/reissue without another change for a duplicate; count derives from the epoch delta |
| Loss completion | New C/passive target, cleared loss T, predecessor anchor and one successor allocation | Delete predecessor normally, then create/build only that successor |
| Predecessor deletion | `predecessorDeleted=true` bound to the retained anchor | Permit successor creation; a crash before this write requires native deletion replay |
| Ready-Idle build | No full build-reply write; accept `admitRepair` with the validated identity/next epoch | Send admission; a lost build reply requires exact native observation/replay |
| Repair admission | New C including the successor; clear repair/T atomically | Admit later work; no old identity is reused |
| Handoff completion | New C and completed latest `switchover` receipt; clear T | Publish the actual selected endpoint; no handoff replay from stale target state |
| Teardown acceptance | Stable exact C/scope and timing, initialized empty receipt sets; no T/repair | Keep routing disabled and admit native shutdown; earlier work must already have finished |
| Native/physical deletion receipt | The appropriate exact-bound set element | Release only the retention/effect justified by that receipt |
| Recovery with complete native deletion | No new progress field; branch from frozen C and receipt sets | Resume Kubernetes/physical cleanup without a native FM handshake |
| Explicit failure/uncertainty | Retain C/T and any necessary boundary evidence, not a timer-created terminal state | Surface the native/validation error; only supported transitions or proven outcomes permit further authority |

Allocation and binding may require multiple reconciliations, but no partially
bound record permits native creation. A deletion timestamp always prevents new
client publication. A status CAS is not atomic with a Service/EndpointSlice
write: retain the [projection fencing rules](operator-restart-design.md#kubernetes-projection-fencing).

On ambiguous writes, read the canonical KubericSet directly and compare its
semantic values/owner before effects. ResourceVersions are opaque CAS tokens.
Never infer a failed commit from a lost HTTP response or blindly merge a
stale completion over a newer accepted target.

## 5. Reconstruction contract required from fube

These requirements are deliberately distinct from Kubernetes watch delivery.
The current [fube limits](../fube/docs/cluster/ExternalFmContainerNode.md#failure-behavior-and-limits)
still gate FM restart/reconnect.

| ID | Required native fact or behavior | If unavailable |
|---|---|---|
| R1 | Authorized FM-instance/generation takeover, including a handover path while the old TCP session remains open, while retaining original Node/RA/RAP/Hosting/application owners | No native-control restart recovery; cleanup-only exception below |
| R2 | Nonempty, exact-incarnation discovery plus configuration/lifecycle observations across the new control session | Do not pretend empty bootstrap or Kubernetes Ready reconstructs native state |
| R3 | Query/replay actual create/build and final reconfiguration outcomes for retained C/T and allocated replica, distinguishing pending, success and native failure | Block that interruption cut; inventory presence is not completion, and terminal failure is not a generic retry |
| R4 | Recover the frozen/native-approved replica descriptions needed for strict command retry and completion validation, including endpoint/LSN handling | Do not synthesize descriptions or weaken equality checks to omit more fields |
| R5 | Replay exact normal-deletion success until its durable consequence; after complete native deletion, allow explicit original Node/supervisor shutdown to drain safely despite FM transport loss, without new FM ACKs | Do not derive deletion/physical receipts from absence; gate unsupported post-deletion closure cuts |
| R6 | Define cross-session duplicate/stale report and completion handling using the retained authority window | Do not promise exact historical replies or adopt superseded completion as current authority |

Initial `NodeUp`/`ReplicaUp`, LFUM generation upload and final RAP completion
remain separate facts. No persisted per-Node upload ACK bitmap is needed if
the native owner can repeat its correlated upload. An ACK that would discard
the last needed fact must first be folded into C/T or a compact receipt.

Existing exact-replay deques may remain in memory. Lower epochs cannot grant
authority, but a lower epoch alone is not proof that a message was once
authorized. Apply the defined native stale/reply contract; do not turn every
old or malformed message into a successful historical acknowledgement.

R5's post-deletion clause is a composed closure contract to establish, not a
claim that fube lacks every disconnected-shutdown capability. Drain or safely
cancel outstanding transport/request ownership without inventing native
completion, and require the original supervisor's exact positive cleanup
report. Complete deletion receipts avoid R1 handover in cleanup-only recovery;
they do not waive physical cleanup or child-object mutation fencing.

The small schema is therefore a conditional design with named native
dependencies, not a proof that today's fube exposes every required read.
If a requirement cannot be supplied, explicitly add the smallest missing
semantic receipt or keep the affected feature blocked. Do not silently add
raw wire history, an event journal or another application owner.

## 6. Current FM field-by-field disposition

This inventory covers every field of the current
[`State`](../kuberic-fm/src/state.rs#L129-L154), not a serialization plan.
Nested state is classified separately where needed.

| Current field | Disposition in the proposed contract |
|---|---|
| `config` | Reconstruct from accepted allocation/bindings, immutable profile and current owner; do not duplicate Config |
| `primary` | Derive from C's existing `primaryNodeId`; use the fixed initial Primary before C |
| `initial` | Rebuild from immutable service/partition, original assignments and versioned initialization recipe |
| `node_up` | Observe again through R1/R2; no full `NodeUp` persistence |
| `bootstrapped` | Reconstruct native discovery readiness; Pod readiness is not a substitute |
| `primary_reply` | Re-query/replay actual Primary creation through R3/R4; no copied reply body |
| `builds` | Re-query/replay exact built Idle outcomes; derive deterministic build order, not a persisted vector/cursor |
| `configuration` | Regenerate normalized T from its tagged intent, C and repair/handoff facts; obtain required native wire view through R4 |
| `configured` | Retain only C's epoch, Primary and active identities; re-observe endpoint/progress details and final native evidence |
| `reconfiguration` | Derive from loss T and whether the predecessor was C's Primary |
| `losses` | Fold accepted loss into the repair predecessor/epoch anchor and T before ACK; no raw inventory body |
| `retired_losses` | In-memory exact-reply optimization; no lifetime durable list; cross-session behavior requires R6 |
| `identities` | Reconstruct active identities from C, original IDs from allocation and the one outstanding repair allocation |
| `repair` | Persist predecessor, successor allocation and its compact deletion receipt; `built` is re-observed, not copied |
| `switchover` | Reuse existing latest typed `status.switchover`; commit acceptance/completion with T/C |
| `pending` | Derive operation/destination/body from C/T/native reality; pacing and per-attempt timers remain local |
| `completed` | In-memory exact-reply cache; native replay/C/T determine cross-session outcomes, not a serialized deque |
| `candidate_changes` | Persist only the latest loss T; derive the per-failover budget from its epoch delta; no raw report history |
| `deleting` | Derive from admitted teardown or the exact predecessor-delete obligation; "sent" is not a durable outcome |
| `deleted` | Compact `teardown.nativeDeleted` boundary receipts, bound to frozen C |
| `disconnected` | Re-observe sessions; never treat a remembered disconnect as physical cleanup |
| `stopping_at` | Derive from accepted teardown; current-attempt shutdown timing remains local to the responsible owner |
| `now` | Process-local clock; never serialize |
| `error` | Re-observe/classify failures and retain necessary source evidence; no blanket durable timer/error latch |

In particular, `Pending.sent` is not a stored send receipt; `Repair.built` is
not a durable callback stage; `CandidateChange.report/request/reply` is not
a persisted history; and a lost local FM join handle is not proof of cleanup.
Reconstruction must use a real restore/evaluation path, not fill these fields
by feeding manufactured `AddPrimaryReply`, build or configuration success
messages into `State::new`.

## 7. Bounds, validation and implementation gates

- `members` and each deletion/cleanup set are bounded by the three original
  Nodes/Pods. There is one accepted intent, one repair and one latest handoff
  receipt, not one object per completed operation.
- Node IDs and Pod UIDs must resolve to exact immutable original bindings.
  IDs/instances, epoch arithmetic, uniqueness and all tagged-union invariants
  are checked before effects. Missing existing authority does not mean fresh.
- New C and its public epoch/Primary, repair retirement and handoff receipt
  are updated atomically wherever their meanings depend on each other.
- Enforce an encoded-size limit below Kubernetes object/request constraints.
  Do not evict an unresolved identity or receipt to make room.
- No extra independent high-water, redirection-count, role, PC-membership or
  current-Primary field is introduced where the rules above derive it.
- The native data-loss epoch and Node-instance recipe belong to the immutable
  profile version. Local retry timing cannot alter configuration authority.
- Public diagnostics, old teardown stage/cohort copies and old in-memory
  backend admission cannot override this versioned authority contract.
- Observers never patch parent status or child finalizers. The reconciler
  alone commits cleanup receipts; installation/release of report retention
  are explicit child-fenced effects, not hidden observation side effects.
- Before enabling an interruption cut, demonstrate its R1-R6 dependencies and
  commit/replay behavior. Cleanup-only needs R5 closure/receipts, not a native
  R1 handshake with retired Nodes. Stable-cohort restart can be delivered
  before the full interrupted-lifecycle matrix; unimplemented cuts remain
  explicit.

This is a documentation design increment. No runtime tests, new apply barriers,
CRD migration, Node replacement, resize, data-loss policy or protected
replicator/provider method changes are introduced.

## Implementation evidence

The decisions above were checked against:

- [FM state, command construction, completion, loss and repair](../kuberic-fm/src/state.rs).
- [Immutable service/profile and generation construction](../kuberic-fm/src/config.rs).
- [Existing KubericSet allocation, status and teardown types](../kuberic-operator/src/crd.rs).
- [Current handoff/routing reconciliation](../kuberic-operator/src/plan.rs#L562-L700).
- [Native deletion and physical-cleanup gates](../kuberic-operator/src/teardown.rs).
- [Cleanup receipt predicate](../kuberic-operator/src/process.rs)
  and [retained Pod termination reports](../kuberic-operator/src/processes.rs).
- [Native replica/epoch description](../fube/crates/libs/fube-ra/src/protocol.rs)
  and [configuration request/reply codecs](../fube/crates/libs/fube-ra/src/protocol/configuration.rs).
