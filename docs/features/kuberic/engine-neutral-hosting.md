# SF Hosting: Current Baseline and Proposed Extension Boundary

**CURRENT:** the bounded common-host port below is delivered. **PROPOSED:**
the [replacement extension boundary](#proposed--ordinary-engine-boundary) is
design only, **NOT IMPLEMENTED**. This documentation change implements no
runtime behavior. [Current semantics](classic-v1-runtime.md) remains the
behavioral authority; the [project roadmap](sf-v1-semantic-alignment-plan.md#hosting-redesign-roadmap--not-implemented)
is the single future implementation queue. Do not apply the proposed contract
to an existing factory.

## CURRENT — delivered bounded hosting

This is the bounded common-host responsibility port on top of merged #198
(`2f97d86605ce69f3812c7d5ba1dd9c6d606ca8d7`), with initial code phases through
`91a2a93c3c7cf9477adb3a0d27e674b99f741a8a` and the linked final-review
retirement, achieved-receipt and query-fencing corrections. Native reference:
Service Fabric [`3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d`][native].
It is source-guided Rust/Kubernetes behavior, not native executable equivalence,
COM/wire compatibility, wholesale RA/FM/Hosting2 or production certification.
Merged #199 delivered this bounded work; #200 delivered the nonempty public
copy-context witness and #201 moved its fixture into the workspace. None is
pending implementation. Their acceptance did **not** establish full SF
custom-replicator extension-boundary alignment.

## Owners and independent facts

Both the built-in engine and independent PostgreSQL allocation attach the same
private [creation-bound coordinator][coordinator]. It owns serialized exact
grant install/revoke, retained response-independent actions, compensation,
receiver history and actual drain results. The [built-in effects][production]
own queues/carrier/provider joins; the [independent adapter][companion] invokes
the engine's own ingress and shutdown. An independent engine does not borrow a
built-in allocation, private attachment or native diagnostic owner.

The common [operation registry][operations], [instruction executor][program],
[admission projection][admission] and [selected access owner][access] retain
request authority, achieved receipts and lifetime policy. There is no parallel
readiness ledger or second topology scheduler.

| Report/control fact | Meaning | Does not prove |
|---|---|---|
| `configuration.voting` | Successful host installation for exact current history/storage/process | A secondary executed a primary-only configuration callback; remote admission; serving access |
| `remote_builds` | Exact journal-derived Accepted / InBuild / Ready / InDrop / Dropped | Observed engine idle membership or a fresh live copy grant |
| `engine_idle = None` | Optional query unavailable or discarded as incoherent | Observed empty membership |
| `engine_idle = Some(... idle_replicas: [])` | Actual empty query, coherently bound to configuration/history/storage/process/creation | Copy readiness or application durability |
| Successful Build completion | Exact retained operation/attempt succeeded | Controller already accepted it, revoked ingress or admitted a voter |
| Positive containment | Successful actual owned drainage | Successful child Close or erased child error |

The [report types and validation][report] keep these facts separate. Torn,
late, duplicate or voter-overlap samples suppress only the optional sample,
not truthful installed host authority. Ready requires exact successful retained
completion; retirement, supersession and terminal authority dominate old success
([projection][remote]). No projection installs a grant.

Installation derives from actual successful configuration instructions, not a
lifecycle input descriptor. Epoch-only recipes do not install configuration.
Current-only receipts exclude previous-only peers. Optional idle samples are
omitted across unfinished/newer native topology effects and operation-revision
changes, including native callback success before its host receipt.

Successful exact voting installation transfers a Build's logical ownership to
configuration. Its projection is Dropped/retired as an **idle Build**, even if
physical slot garbage collection is lazy; later voter withdrawal cannot
resurrect that old copy as Ready. This does not mean the native voter vanished
or joined. Unretired Ready idle ownership still requires exact withdrawal,
without inferring native membership from completion.

Native V1 manager idle membership can exist while copy is still running.
PostgreSQL's native `built_idle` observation publishes only completed copies.
Both meanings are preserved. Neither is a universal readiness certificate.
Native FUP likewise owns InBuild/Ready/InDrop separately from engine queries;
native internal-query support is optional.

Native RAP's Build/Remove request view describes the remote replica as idle
even when other control-plane descriptions carry a voting role. Local explicit
Build/Remove intents likewise do not implicitly admit a voter.

## Pinned message → action → state mapping

Native paths below are under `src/prod/src/Reliability/Failover/ra/` at the pin.
Ranges are inspected source evidence, not executed native tests.

| Native message/owner | Source behavior | Local owner and intentional adaptation |
|---|---|---|
| `Message.h:20–51`, `ProxyRequestMessageBody.h`, `ProxyReplyMessageBody.h`; RAP dispatch `ReconfigurationAgentProxy.cpp:180–280` | Named Open/Close/Reopen/UpdateConfiguration/Build/Remove/epoch/status/cancel/query; request carries runtime, FU/local/remote/service/flags | Public control programs carry exact identity/process/history/storage/epoch/configuration/attempt. Durable acceptance and typed results replace native IPC/no-reply retry; not native wire serialization. |
| `FailoverUnitProxyContext.h:58–149,194–252`; `FailoverUnitProxy.cpp:133–250`; action compatibility table | Revalidate epoch/instance/state; incompatible actions wait/retry; close business before cancellation | Registry plus transactional admission rejects stale/changed identities, serializes incompatible authority and permits compatible unrelated Builds. Exact active duplicates attach; completed duplicates replay immutable results. |
| `ComProxyStatefulService.OpenAsyncOperation.cpp:17–39`; `FailoverUnitProxy.OpenReplicaAsyncOperation.cpp:112–162`; `ComProxyReplicator.h:17–35` | Service Open returns coherent required interfaces; internal query and specific-quorum interfaces optional | One-shot factory reservation and coherent interface registration; explicit supplemental ownership, optional diagnostics and existing specific-quorum capability. Protected replication/provider contracts unchanged. |
| `ProxyActionsList.cpp:34–193` | Ordered Open, promotion/demotion, catch-up/current configuration and role/access actions | Frozen instruction programs and controller-composed stages; successful prefixes are distinct from pending/failed callbacks. Paired roles, epoch, exact configuration and fresh promotion catch-up qualify access. |
| `ReconfigurationAgentProxy.cpp:761–974`; `FailoverUnitProxy.cpp:636–920` | State-sensitive UpdateConfiguration selection; pending versus completed configuration; no older joint catch-up after completed current stage | Common gate checks all actually selected configuration stages, including Swap's starting set. It applies to primary engine configuration, not a secondary's host-only transport installation. |
| Same native UpdateConfiguration selector | Current-stage peer updates differ from stale Catchup-flag messages | Fresh current-only synchronization supports planned handoff, recovery and renewed/built peer admission at frozen authority. Renewal does not reopen PC/CC after finalizing a retained subset. Selected catch-up cancellation still requires a newer epoch. |
| `FailoverUnitProxy.ReplicatorBuildIdleReplicaAsyncOperation.cpp:15–217,256–357`; `FailoverUnitProxy.cpp:960–1064` | FUP tracks exact remote operations/tombstones; accepts success only while still InBuild; Remove/cancel invalidates late completion | Existing exact Build/retirement journal projects remote state; source Build needs achieved paired Primary and installed authority. Exact Remove and supersession fence callbacks. Failed Build does not automatically restart. |
| `FailoverUnitProxy.cpp:1483–1664`; user API invoker `:50–64` | Cancellation can race Begin; drain waits owned work | Retained Rust roots/control tasks and compensation joins outlive RPC observers. Cancellation invalidates access and aborts **and joins** roots; engine descendants require their declared ownership/drain boundary. |
| `ReconfigurationAgentProxy.cpp:1386–1456`; Close child operation `:78–111` | Native outer Close may Abort and send success after child failure | Deliberately different local result: abort/join still runs, but child failure remains a failed public completion even after positive containment. Failed/lost drain never becomes positive containment. |

This current implementation map covers relevant RAP/FUP/common-proxy semantics, not full RA
reconfiguration stages, FM placement, Hosting2 process management, native
compatibility tables, settings or automatic recovery policy.

Joint voting admission after a completed current-only stage needs a new
reconfiguration epoch, with actual child epoch and peer transport authority
advanced coherently; a newer operation revision alone is insufficient.
Standalone Swap's pre-revoke configuration/catch-up runs **before** its epoch
advance (`ProxyActionsList.cpp:130–142`). The peer must retain the starting
channel until that wait finishes, then install the refreshed authority for the
second wait. Controller-composed Swap has already performed the first wait.

## Acceptance, retry and access

Exact input digest, operation ID/revision, resource, process session, storage,
history, epoch, configuration and Build attempt remain fenced. Higher numeric
revision cannot revive lower epoch or selected cancelled catch-up. Ordinary
duplicate failure is not universal success.

Epoch checks cover each actually executed configuration/wait stage, not merely
a Swap's final epoch. A non-composed Swap captures compatible installed
starting PC/CC and exact peer bindings; a newer final epoch cannot hide an
older starting callback. Controller-composed Swap omits that stage, so unused
starting metadata is not replayed as an action.

Exact Remove withdraws the unchanged retained Build token; that token's older
epoch is not a request to restore old authority. While Build is active, Remove
uses its revision and still waits for successful containment. After containment
or completion and authority advance, Remove uses current source authority.
Unresolved incompatible ownership blocks preparation rather than authorizing
a newer Remove or replacement Build. Exact session, storage, history and attempt
checks still fence cleanup from a successor.

The journal has a nonterminal `RetryPending` boundary for a **proven, selected
returned-callback failure**, with immutable per-attempt waiter results and exact
successful-prefix restoration into a new fenced access generation. Earlier
waiters cannot erase results or revoke later generations. Rebuild obligations,
unknown/host/receipt failures, interruption, data-loss ambiguity, Build and
destructive terminals do not borrow that proof.

**Current production engines declare no returned-failure repeatability.**
Positive failed-prefix resumption is qualified with explicit private fixtures;
actual V1/PostgreSQL undeclared application failures remain containment-required.
`CallbackOwnership::NativeObserver` is not repeatability or successful drainage.
It applies only to the engine's selected callbacks, never independent
application Role/Open or the application's Access refresh. The conservative
default is `ObjectOwnedOnInterruption`.

Promotion receipt restoration never imports unrelated historical catch-up.
Secondary preparation, idle Build and one successful role child never grant
serving access. Native epoch/progress observations retain their real meaning;
descriptor readiness is not live health or application commitment.

## Controller copy, renewal and retained lifetime

Production dispatch freezes exact target authority, awaits its installation,
then calls source Build. Source completion is distinct from controller
acceptance: exact revoke precedes separate joint/current voter admission.
Revoke withdraws only matching ingress, not receiver ownership, existing
replication or voting configuration.

Renewal keeps its command/access fence while admitting only the exact
ConfigurationPending replacement copy needed to finish that renewal. Exact
predecessor containment, replayed replacement role, source/target endpoints,
process/creation/storage/history/epoch remain required. This is not a general
fenced-operation bypass. After lost replies/controller recreation, the
[renewal owner][renewal] reconstructs only exact compare-revoke from retained
frozen Build input plus successful completion before further configuration.
Historical completion never recreates ingress.

Close/Abort stop admission and revoke access before actual retained joining.
For a native-observer engine, cancellation joins the operation root and checks
the exact durable callback selection. A gap between successfully journaled
callbacks has no interrupted child and does not require whole-runtime shutdown
before a successor can run. A canonical rebuild receipt still denotes a failed
callback, not a successful gap; its application/native ownership is retained
even when the receipt clears the in-flight selection. An in-flight application
callback, including one
whose successful return lost its receipt, cannot borrow the engine's ownership
declaration; unknown store evidence remains conservative.
A monotonic current-process terminal fence is distinct from renewable access
fencing: neither an old renewal snapshot nor later renewal authorization can
reopen copy after valid terminal admission begins. Target validation rechecks
that current fence at every success exit, including preinstall classification.
Persistence uncertainty remains fail-closed; contained predecessor-session
terminal records do not permanently fence a fresh replacement process.
A lost response does not discard installation, cleanup or compensation.
Returned drain errors and lost native task joins are sticky across later
observers. Protected Close/Abort must independently wake blocked companion
actions before serialized drain, avoiding installer/drain dependency cycles.

## Compatibility and qualification

Host metadata replacement schema is **14**; schema 13 is deliberately rejected,
not silently upgraded or defaulted into new authority. Application/provider
data formats are unchanged. The unstable Public V1 report/wire adds separate
optional `engine_idle` and exact `remote_builds`; the host configuration wrapper
no longer embeds idle membership. No rolling old/new control-wire guarantee is
claimed.

Executed Rust qualification includes:

- [Actual V1 controller journeys][v1-tests]: held real nonempty copy, native idle
  before completion, exact remote state/grants, separate voting; existing real
  failover/switchover/recreation/renewal/isolation and retained observer paths.
- [Actual independent PostgreSQL controller journeys][pg-tests]: backup/lineage/
  receive/replay, old/new SQL fencing, exact retries/revocation/invalidation;
  hosted public terminal success, contained child failure and lost real join.
- Both engines' undeclared callback failures remain fail-closed through exact
  production redispatch. Direct negative fault cuts are setup, not substitutes
  for controller dispatch.
- Host/store/wire/coordinator and synthetic companion negatives separately
  qualify stale/torn/missing observations, immutable attempts, failed receipts,
  cancellation, retained joins and exact scope. Seeded records are identified
  as fixtures, not native executions.

Use the [explicit validation commands](testing.md#engine-neutral-hosting)
for behavior and public API/privacy/Rustdoc/production package guards. Filtered
nextest discovery is not evidence that source/ignored guards ran.

No native binary differential, arbitrary failed-Build recovery, unpublished
MySQL qualification, broad security/settings expansion, kernel power-loss
or production certification is claimed.

## Source-confirmed extension-boundary difference

Native source is pinned to
`3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d`; current Kuberic research is at
`b37009f8af7a7b2ed91f58f4889ed74e06013861`. These are inspected source,
**not native executable/differential evidence**. The native provided engine's
algorithm is not a requirement that independent engines use that algorithm.

| Source-confirmed fact | Exact source evidence |
|---|---|
| Service Open supplies the ordinary replicator; host requires replicator and primary interfaces; internal status queries are optional | [Open:11–39][sf-open], [QI:17–37][sf-qi], [public IDL:606–763][sf-contract], [internal query:230–235][sf-query] |
| Managed custom engines have an ordinary broker path; cancellation is advisory and may still complete | [broker:14–121][sf-broker], [IReplicator:11–141][sf-managed] |
| FM sends AddReplica/RemoveReplica/reconfiguration to RA; RA coordinates remote creation and endpoints before source RAP Build | [FM:116–173][sf-fm], [RA Add:1848–2041][sf-ra-add], [Create reply:2851–2924][sf-ra-create] |
| RAP selects action lists; FUP owns remote instance state, retained operation/cancellation and stale completion; ordinary Build is the engine call | [RAP:977–1031][sf-rap-build], [actions:34–189][sf-actions], [FUP:15–358][sf-fup], [Begin/End:12–48][sf-build] |
| RA revalidates Build replies before remote readiness/FM credit; host access is separate from Build and engine progress | [RA reply:4027–4091][sf-ra-reply], [access:33–194][sf-access] |
| Provided engine owns StartCopy/context/session checks and copy+replication completion, not target controller-attempt grant installation | [source:22–222][sf-primary], [target:827–1080][sf-secondary] |
| Host drain waits action lists; concrete engines own their native callback/queue waits; persisted RA reopen refreshes instance context | [FUP drain:1483–1650][sf-drain], [primary Close:85–141][sf-close], [secondary Abort:452–510][sf-abort], [reopen:3795–3847][sf-reopen] |
| Current Kuberic hosted controller Build installs target authority and fails without native runtime control | [dispatch:3310–3356][current-dispatch], [host:891–927][current-install] |
| Current carrier and real PG enforce additional exact attempt/host authority around native effects; PG is an independent allocation | [carrier:149–259][current-binding], [PG factory:314–362][current-pg-factory], [PG Build:2368–2701][current-pg-build] |

The mandatory companion is a real **extension-boundary divergence**, not
automatically justified by “Kubernetes/Rust adaptation.” Native FUP's source
Build state is not a target-side grant callback. Conversely, deleting the
callback alone would leave controller/store/carrier dependencies and remove
real fences. The redesign replaces their safety owners, not just their names.
See [source provenance and notices](sf-v1-port-provenance.md) and the retained
[MIT attribution](../../../THIRD-PARTY-NOTICES).

## PROPOSED — ordinary engine boundary

Everything below is a **design decision, NOT IMPLEMENTED**, unless explicitly
identified as source evidence above. Public `Replicator`, `PrimaryReplicator`
and `StateProvider` method sets stay unchanged. Breaking other APIs/formats is
allowed only in future separately authorized implementation.

### Decision and alternatives

Select controller → local replica agent → host proxy → ordinary engine,
separating source remote-operation state from target engine copy sessions.
These are components in the existing crates, not new services or a full native
RA/FM/Hosting2 port.

Rejected alternatives:

- Retain/relabel `CustomReplicatorControl`: keeps the extra extension boundary.
- Move identical callbacks to another public trait/factory closure: same
  mandatory companion, not alignment.
- Delete grants with no replacement: loses stale-session and destructive-effect fences.
- Port native IPC/COM/FM/Hosting2 wholesale: exceeds the relevant responsibility
  alignment and does not fit existing Kubernetes placement.

Retain local durability where needed, but do not freeze present journals,
reports, wire fields or coordinator policy as architecture.

### Owners and state

| Owner | Target responsibility and authoritative state |
|---|---|
| Controller (FM-like responsibility) | Desired topology/placement, epoch/PC/CC and failover/handoff goals; persist goal revision, exact participants and accepted authoritative outcomes. No target engine grant commands or diagnostic-derived authority. |
| Local replica agent (RA-like responsibility) | Durable replica/storage identity, highest epoch/configuration, role intent, process/endpoint publication and reconfiguration stage; coordinate target creation, choose host action lists, reject stale goals. No PG-specific certificate fields. |
| Host proxy (RAP/FUP-like responsibility) | Coherent creation, action compatibility, retained ordinary calls, exact outcomes and access; source remote table keyed by target instance plus both process/creation/storage identities, epoch/scope and host-local generation. |
| Engine | Real endpoint, authenticated peer sessions, target receiver, source copy boundary, context/replication/ACKs, native data/lineage, idle membership and actual descendant teardown. Engine transfer IDs are not controller attempt IDs. |
| Service/provider | Application durability, snapshots, ordered commitment/publication and access enforcement. PG enforces physical SQL fences, not merely host status. |

Remote host states: Absent → InBuild → Ready → InDrop → Dropped. Accepted
selection is not Ready. Only successful ordinary Build plus current
role/epoch/generation and **both endpoints'** creation/process/storage
revalidation yields Ready. Preserve remove-before-Build tombstones. Optional
engine-idle membership cannot manufacture any transition or access grant.

### Creation, discovery and target establishment

Service Open returns one coherent ordinary factory bundle. One reservation
binds local replica/storage/process identity, listener, cancellation lifetime
and at most one provider writer. Mismatched/abandoned creation is fenced and
settled. Engine Open owns and returns its real endpoint **before** peer-dependent
role recovery; pending recovery must not block the connectivity it needs.

Remove `with_custom_control` and public outbound Build-authority accessors.
Keep read-only partition/lifecycle/creation context and generic peer discovery.
The local agent publishes the fresh endpoint immediately after successful Open
as **DiscoveryPending**, before any peer-dependent role/epoch/configuration
callback must complete. Authenticated discovery and engine-owned recovery/status
traffic can progress; ordinary copy, voter admission and service access remain
closed. Accepted role intent is not an achieved role. After applicable ordinary
callbacks and their host receipts complete, publish **AchievedScope** eligible
for the Active-session checks below. A peer-scope snapshot contains resource,
replica/storage/process/creation, endpoint, accepted epoch/primary/configuration
scope and monotonic publication generation. It contains **no** controller
operation ID/revision, Build ID or exact inbound grant, and invokes no engine
install/revoke callback. Engines can use the built-in carrier or their own
protocol with equivalent authenticated peer checks.

Target preparation first opens/publishes DiscoveryPending, then completes
ordinary epoch and Idle role under closed access and publishes AchievedScope
with current generic source scope. A secondary host records that scope without issuing a
primary-only configuration callback. Engine establishment checks current local
role/epoch/creation, authenticated current source Primary identity, endpoint and
compatible history; a bearer credential or copied descriptor alone is insufficient.
Discovery failure remains pending/closed.

### Selected scope fence and engine session retirement

DiscoveryPending publication linearizes after successful Open and permits
discovery/recovery only; AchievedScope publication linearizes after the matching
ordinary callback receipts. Neither permits arbitrary future effects. Compatible
same-creation configuration changes use ordinary callbacks. An incompatible
primary/history/storage/epoch scope change:

1. Agent unpublishes the old endpoint and denies access.
2. Host calls ordinary Abort; the engine synchronously invalidates its local
   lifecycle generation under its own effect-admission gate.
3. Agent waits retained ordinary Close, host and application drainage, or
   independently verified process containment.
4. Only then acknowledge withdrawal/replacement and permit new scope/storage reuse.

No silent registry update bypasses this barrier. Lost/failed settlement stays
pending. This coarse ordinary-only rule costs availability but introduces no
registry-to-engine callback companion.

Each engine owns source states Proposed → Active → Retiring → Retired and
target states Reserved → Receiving → Completed → Retiring → Retired:

- Target reserves a session/receiver **before** asking the authenticated source
  engine to confirm that session Active for both creations. Source confirmation
  serializes with retirement; a merely live Primary is insufficient.
- Source retirement records an engine-session tombstone, cancels sending/context
  work and retries target cancellation. Target fences effects, joins owned work,
  records retirement and acknowledges only after settlement.
- Duplicate establishment attaches/ACKs only the same live session. Cached
  completion rechecks both creations, source Active and current native lineage.
  Retired sessions cannot reopen. Tombstones last for the creation lifetime;
  reopen invalidates every old session through fresh process/creation identity.
- Delayed first handshake after source retirement fails Active confirmation.
  If confirmation won first and cancellation is lost, target may still own old
  work: **source Remove is not remote containment**. Target remains unadmitted/
  quarantined, cannot reset for another receiver, and cannot publish copy credit
  without final Active confirmation. Reuse requires actual target retirement/join
  acknowledgement or target Abort→Close/replacement, not a disconnected socket.

Bind engine session nonce/generation to both endpoint, replica/storage/process/
creation identities, history and epoch. Context/copy/replication/ACKs must match.
Higher epoch never silently reinterprets old bytes. Accepted built-in provider
callbacks may finish after a fence but cannot install/publish into a successor;
Close waits before releasing the writer. PG invalidates its local generation
before PGDATA/helper effects, rechecks after awaits and final Active confirmation,
and retains exact backup/WAL/process/gateway join owners. An already-issued
irreversible effect is not rolled back by a flag: quarantine until settlement.
Engine-local session retirement is not controller-attempt authorization.

Built-in copy preserves receiver-before-context establishment, target-origin
context including empty items/EOF, source state at start-minus-one, and final
copy **plus** replication boundary ACKs. PG retains its own backup/slot/WAL/
receive+replay/checkpoint protocol; it need not imitate built-in queues.

### Controller-to-host messages and action selection

These are logical proposed messages, **not delivered API signatures**:

| Request / direction | Preconditions, action, completion and stale rule |
|---|---|
| ReconcileReplica: controller → local agent | Goal revision, identities, epoch, PC/CC, role/stage. Persist accepted goal; reject stale/changed identity. Return achieved stage or pending, not engine diagnostics as proof. |
| PrepareIdle: agent coordination → target host | Open real endpoint and publish DiscoveryPending before peer-dependent callbacks; complete epoch/Idle under closed access, then publish AchievedScope with generic identity/source scope. Completion is prepared creation, not copy. Failed preparation withdraws through the same Abort→Close barrier. |
| BuildIdle: source agent → source host | Paired Primary, current epoch and exact target publication. Retain ordinary Build and source remote state. Active duplicates attach. Ready replay requires unchanged **both** endpoint creations/process/storage and scope, revalidated before voter admission. |
| Cancel/RemoveIdle: agent → source host | Fence generation first. Ready/Open/Primary: ordinary Remove retires local engine transfer; target settlement is separate. InBuild: portable Abort→retained Close/drain→retire/reopen; never ordinary Remove on an aborted allocation. Late success cannot replace InDrop/Dropped. |
| Reconfigure: agent → local host | Select ordered epoch/data-loss/role/catch-up/configuration/access actions from goal/stage and compatibility. Successful prefixes reusable only in unchanged generation. Secondary scope installation is not a primary-only callback. |
| CloseReplica / AbortReplica: agent → host | Stop admission/access before cancellation; retain every call/cleanup owner. Claim only achieved postconditions. |
| QueryReplica: host → agent/controller | Achieved role/configuration, source remote states, pending/failure; optional coherent engine diagnostics remain separate. Query creates no authority. |

Controller does not schedule an opaque instruction list as placement policy;
agent selects ordered actions. Permit compatible concurrent Builds/configuration
only while actual epoch/role/identities remain valid. Higher revision alone
cannot bypass epoch, ambiguity or an incompatible action.

End-to-end: target creation → source ordinary Build → engine session/context/
copy+catch-up → callback success → host Ready after revalidation → exact
agent/controller accepted outcome → **separate** joint/current admission and
access. No target install/revoke callback occurs. Admission retires logical idle
ownership without destroying admitted replication; voter removal is configuration
first. Successful copy is not serving access or storage-deletion permission.

### Cancellation, Close, Abort and real joins

Remove `callback_ownership` policy selection. Host retains **every** accepted
ordinary future and immutable result independently of RPC observers until
End-like completion; response loss never drops accepted callback work.
Cancellation invalidates generation/access first and is advisory. Portable
InBuild cancellation always uses whole-creation Abort and retained Close.
There is no inferred selective cancellation capability or new protected method/
token. Engine-private cooperative cancellation may shorten cleanup but is not
a host integration interface.

**Explicit proposed Rust lifecycle obligation, not a universal native IDL
guarantee:** successful ordinary Close settles the engine endpoint, receivers,
provider interactions and descendants. Abort synchronously stops admission and
initiates native fencing/cancellation. Engine retains a terminal join owner;
ordinary Close remains awaitable after Abort and observes the same outcome.
This moves current built-in facade/transport joining into ordinary lifecycle
implementation, not another public `shutdown_and_drain` trait.

Host drains host calls; engine Close drains engine work; application Close
drains application work. Release provider writer only after required success or
verified process containment. Error/hang/lost join means containment pending,
closed access and no conflicting storage use; escalate to verified managed
termination/reaping, not signal-only success. PG must stop/reap actual PG/helpers.
Host action drain proves no arbitrary descendants; an unreachable supervisor
cannot certify orphan death. Keep failed-child-Close as failed public outcome
even after actual successful cleanup. Never repeat an ambiguous destructive
callback merely because its reply was lost.

### Recovery, access and retained state

Retain durable generic epoch/identity/PC/CC/goal/stage, canonical operation
selection/outcome, successful prefix, ambiguity and retirement properties.
Replace instruction-journal layout; it is not immutable protocol. Keep source
remote results/tombstones until accepted incarnation/epoch rejection watermark
permits compaction; bound storage, never TTL-forget unresolved ambiguity.
Remove target grant journal/reauthorization receipts. Host reports do not
replace engine data/history.

Restart unpublishes process-bound endpoints/sessions and stays closed. Open
endpoint, publish fresh process/creation as DiscoveryPending, reconcile ordinary
epoch/role/config and fresh native recovery/quorum evidence, then publish
AchievedScope before copy/admission eligibility. Multiple replicas may resolve
one another while role recovery is held; discovery never waits for that callback.
Failed preparation safely withdraws the new publication through Abort→Close.
Old in-flight Build is uncertain,
not replayable success. Same-live-host lost-reply recovery may return completed
outcome only if **both endpoints** and scope remain unchanged. Target reopen
invalidates Ready even with stable replica instance; controller revalidates
target publication before admission. Otherwise retire/recover/rebuild only after
containment. A durable goal does not resurrect a task.

Preserve native-shaped PC/CC and selected catch-up predicates; do not adopt the
superseded PG fresh-invocation-floor proposal as current/native contract.
Host access requires paired roles, completed selected epoch/configuration/
catch-up, live quorum and unfenced current action. Idle lists, scalar progress,
successful Build or receipts alone are insufficient. PG owns lineage, policy
intersection, receive/replay, promotion and SQL fencing; incompatible history,
missing acknowledged-prefix carrier or unsafe recovery quorum remains closed.
Marked-successor PG WriteQuorum stays unsupported; current All/capability
fallback is not removed by this redesign.

### Mechanism disposition

| Present mechanism | Target disposition | Replacement owner / rationale |
|---|---|---|
| Ordinary protected traits; coherent Open | **Retained** | Native ordinary extension boundary |
| CustomReplicatorControl / with_custom_control | **Removed** | Engine-native sessions and ordinary lifecycle; no renamed companion |
| Exact inbound install/revoke, outbound grant accessors | **Removed** | Generic identity/epoch scope plus engine Active/Retired sessions and creation withdrawal barrier |
| Common coordinator receiver/grant policy | **Replaced** | Private host action retention/drain; actual engine receiver/teardown owners |
| Source remote states/tombstones | **Internalized** | FUP-like host instance/generation bookkeeping, not target authorization |
| Instruction journals | **Replaced format; safety properties retained** | Durable goals/achieved stages and exact outcome/ambiguity; justified local restart choice |
| configuration / remote_builds report schema | **Replaced schema; authoritative facts retained** | Achieved host stage and source outcomes scoped to live process |
| engine_idle / internal status query | **Retained optional** | Never access/readiness/empty fallback; bookkeeping works without diagnostics |
| NativeBinding controller-attempt fields | **Removed** | Generic authenticated identity and engine-owned session/epoch/history checks |
| Access/PC/CC/catch-up owner | **Retained/refactored privately** | Host responsibility distinct from native commitment |
| Metadata/CRD/control payload formats | **Replaced/versioned** | Local durable safety; no native wire compatibility claim |
| PG durable metadata, slots, lineage/process ownership | **Retained initially** | Companion removal is not field-by-field metadata-removal proof |

### Migration and unresolved proof risks

The [single project roadmap](sf-v1-semantic-alignment-plan.md#hosting-redesign-roadmap--not-implemented)
orders new dispatch, built-in and real independent PG migration, recovery,
removal and maintenance-only upgrade. Its acceptance gates must prove effect
linearization, source retirement/target quarantine, Close-after-Abort settlement,
data-preserving format transition and no stale Ready reuse. These are unresolved
**implementation risks**, not permission to leave target owners unspecified or
claims that new code exists. Current selectors in [testing](testing.md#hosting-redesign-baselines-and-future-acceptance)
are baselines, not proof of the proposal.

Downstream MySQL remains unqualified: implement ordinary coherent creation,
authenticated native session establishment/retirement, lineage/progress/restart,
actual terminal joins and access fences, then independently qualify real
controller transfer/admission and failures. No unpublished downstream fixture
or reported trace is reproduced here. PG metadata removal is separately
[deferred behind a complete reconstruction gate](../postgres/stateless-metadata-prerequisites.md).
SF-11/mode/PG/P7/security and native differential/production limits remain owned
by the roadmap; this redesign does not close them.

[native]: https://github.com/microsoft/service-fabric/tree/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Failover/ra
[coordinator]: ../../../kuberic-runtime/src/replicator/coordinator.rs#L1
[production]: ../../../kuberic-runtime/src/replicator/v1/service_adapter/production.rs#L223
[companion]: ../../../kuberic-runtime/src/replicator/custom.rs#L1
[operations]: ../../../kuberic-runtime/src/host/operation.rs#L1504
[program]: ../../../kuberic-runtime/src/host/public_lifecycle_program.rs#L10
[admission]: ../../../kuberic-runtime/src/host/public_action_state.rs#L136
[access]: ../../../kuberic-runtime/src/host/selected_access.rs#L580
[remote]: ../../../kuberic-runtime/src/host/state.rs#L315
[report]: ../../../kuberic-runtime/src/protocol/public_operations.rs#L2476
[renewal]: ../../../kuberic-controller/src/cluster_api.rs#L2374
[v1-tests]: ../../../kuberic-level-tests/src/in_process/journeys.rs#L1
[pg-tests]: ../../../examples/postgres/src/public_controller_tests.rs#L186
[sf-open]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Failover/ra/ComProxyStatefulService.OpenAsyncOperation.cpp#L11-L39
[sf-qi]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Failover/ra/ComProxyReplicator.h#L17-L37
[sf-contract]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/idl/public/FabricRuntime.idl#L606-L763
[sf-query]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/idl/internal/FabricRuntime_.idl#L230-L235
[sf-broker]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/managed/Api/src/System/Fabric/ReplicatorBroker.cs#L14-L121
[sf-managed]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/managed/Api/src/System/Fabric/IReplicator.cs#L11-L141
[sf-fm]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Failover/fm/PendingTask.cpp#L116-L173
[sf-ra-add]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Failover/ra/ReconfigurationAgent.cpp#L1848-L2041
[sf-ra-create]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Failover/ra/ReconfigurationAgent.cpp#L2851-L2924
[sf-rap-build]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Failover/ra/ReconfigurationAgentProxy.cpp#L977-L1031
[sf-actions]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Failover/ra/ProxyActionsList.cpp#L34-L189
[sf-fup]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Failover/ra/FailoverUnitProxy.ReplicatorBuildIdleReplicaAsyncOperation.cpp#L15-L358
[sf-build]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Failover/ra/ComProxyReplicator.BuildIdleReplicaAsyncOperation.cpp#L12-L48
[sf-ra-reply]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Failover/ra/ReconfigurationAgent.cpp#L4027-L4091
[sf-access]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Failover/ra/FailoverUnitProxy.ReadWriteStatusCalculator.cpp#L33-L194
[sf-primary]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Replication/PrimaryReplicator.BuildIdleAsyncOperation.cpp#L22-L222
[sf-secondary]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Replication/SecondaryReplicator.cpp#L827-L1080
[sf-drain]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Failover/ra/FailoverUnitProxy.cpp#L1483-L1650
[sf-close]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Replication/PrimaryReplicator.CloseAsyncOperation.cpp#L85-L141
[sf-abort]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Replication/SecondaryReplicator.cpp#L452-L510
[sf-reopen]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Failover/ra/FailoverUnit.cpp#L3795-L3847
[current-dispatch]: ../../../kuberic-controller/src/cluster_api.rs#L3310-L3356
[current-install]: ../../../kuberic-runtime/src/host/hosting.rs#L891-L927
[current-binding]: ../../../kuberic-runtime/src/host/native_transport.rs#L149-L259
[current-pg-factory]: ../../../examples/postgres/src/service.rs#L314-L362
[current-pg-build]: ../../../examples/postgres/src/public_v1.rs#L2368-L2701
