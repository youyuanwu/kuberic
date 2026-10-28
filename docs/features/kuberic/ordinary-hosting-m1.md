# Ordinary Hosting M1

**IMPLEMENTED: generic host/controller contract and bounded M2 integration.**
Built-in V1, KVStore2, SQLite, the external provider and independent PostgreSQL
now use ordinary hosting, engine-owned authenticated sessions and retained
native/transport/application Close. Applicable M2 implementation acceptance
passed through the independent Phase 6 review; documentation implementation
review and actual independent final Sol/Astra gates remain pending.
This is an experimental development checkpoint, not production certification.
See the bounded
[engine](testing.md#built-in-m2-engine-slice) and
[application selections](testing.md#built-in-m2-application-slice), not a claim
that the complete M2 workflow or historical recovery journeys are qualified.
Comprehensive recovery/failover qualification is **M3, not implemented**.
See the [roadmap](sf-v1-semantic-alignment-plan.md#hosting-redesign-roadmap).

The independent ordinary-engine witness exercises the real production
Reconciler, Kubernetes client boundary and authenticated agent gRPC. It is not a
built-in wrapper, companion, diagnostic implementation or test-host dispatcher.
Its Kubernetes API is an in-process storage model, including generated-schema
status pruning; this is not a live-cluster or native Service Fabric execution.

## Owners and production flow

| Owner | Authoritative responsibility |
|---|---|
| Controller | Desired topology and per-replica canonical semantic goals; exact accepted outcomes, separate voting admission |
| Existing local agent/store | Goal admission, current identity/epoch, locally frozen action sequence, selected call, receipt, uncertainty and achieved stage |
| Existing host/registry | Coherent creation, action compatibility, access, retained ordinary futures/results, source remote states and actual host drainage |
| Engine/service | Real endpoint and authenticated native recovery/transfer, application data, effect fencing and ordinary descendant joining |

The [production reconciler](../../../kuberic-controller/src/reconciler.rs#L670)
uses [ordinary reconciliation](../../../kuberic-controller/src/ordinary_reconciliation.rs#L238).
It first retains independent Discover/Open goals for eligible peers. Once their
actual endpoints exist, it retains role/scope preparation, including Prepare
IdleSecondary on a Build target. It then calls ordinary source Build, accepts
exact current Ready, and only later selects joint/current configuration and
catch-up for voting/access. It never installs or revokes a target engine grant.
Fresh metadata provisioning remains an authenticated initialization command;
legacy callback-program, grant and renewal production dispatch rejects.

The [canonical CRD goal](../../../kuberic-controller/src/crd.rs) replaces
controller command JSON. The [local operation owner](../../../kuberic-runtime/src/host/ordinary_goal.rs)
freezes its ordinary actions and receipts in the existing journal, not a parallel
readiness ledger. Controller recreation resumes exact retained goals. A task
does not reappear merely because its durable goal remains.

## Endpoint publication and access

After actual successful ordinary Open and its host receipt, the current creation
publishes **DiscoveryPending**, before peer-dependent role/epoch recovery.
Authenticated discovery/recovery can resolve that real endpoint while ordinary
copy, voting and serving remain closed. **AchievedScope** requires the matching
ordinary callback receipts; accepted role intent is not achieved role.

Routine [snapshots/discovery](../../../kuberic-runtime/src/host/ordinary_goal_service.rs)
do not invoke engine progress, capability or diagnostic callbacks. Independent
implementations retain explicit read-only progress sampling; normal Open does
not require a native-specific facet.

The bounded ordinary23 native inspection component uses a newly selected
**Discover** goal as a GetLSN round, reusing an already opened creation instead
of calling Open again. Its existing operation journal captures the exact
history, logical configuration, primary fence, revision and process/creation
before effects. Actual InBuild/Ready selects UpdateEpoch (unless that fence was
actually applied), current progress, then capability. Actual Opened/StandBy
selects inspection only, with zero capability: it never constructs a secondary
or supplies copy completion, remote ACKs, voting or access. Metadata sampling
returns the retained native observation, not an unjournaled epoch effect.
Opened→ActiveSecondary and copied Idle→Active role completion do not themselves
confirm the requested fence; the explicit native epoch program must still
succeed. Incomplete secondary preparation retains its real epoch failure.

Optional `nativeStatus` and `nativeLifecycle` snapshot fields distinguish
inspection, actual lifecycle state and the lifecycle-produced deactivation
pair. Missing fields/history are not valid election evidence. Native/application
Primary and epoch alone leave the proxy InBuild; the actual selected PC/CC
ReportAnyDataLoss decision produces Ready without calling user OnDataLoss when
its predicate is false. Callback-required cases remain explicit errors on this
bounded path. Normal promotion retains its selected GetLSN input boundary;
configured-source achievement alone does not advance that boundary's history:
the selected native quorum wait must actually complete first.
source demotion captures actual final GetStatus, never a boundary from EndWait
or a later moving sample. These facts are persisted before acceptance and do
not restore a warm native owner after recreation.
Normal source handoff selects local input without requiring the successor
already be Primary. Idle/secondary history inheritance still requires the
actual matching Ready Primary source and its configuration.
Warm secondary preparation does not inherit from a future, not-yet-configured
Primary; genuine new Idle and copied Idle activation keep the source check.

A successful same-live normal preparation may reach current-only admission
only through its exact completed joint operation: original PC, selected CC,
membership/quorum, primary fence, native query/source boundary and current
process/creation must agree. The complete native recipe, acceptance and host
settlement remain required. Every other historical conflict, unresolved call
and sticky failure is still checked. Final admission is not completion: the
same owned final attempt performs catch-up, actual native current/end and
NativeComplete before access and exact request-target acceptance. A later
current-only result does not replace the original source-demotion result.

The former Primary's first incoming transport uses the actual root retained
inside native demotion, after real old local transport/ACK/task joins. It is
neither an Idle Build nor a reopened/copied-state marker. Copied same-fence
normal continuation and strict higher-fence general recovery remain separate.
Selected native member progress initializes the sender, not a fresh wire ACK.
At tail, configuration needs no invented traffic; RequestAck cannot establish
an unknown primary route. Real replication installs that route and subsequent
writes require native consumer ACKs and quorum. A healthy unmarked copied
member behind the boundary retains its root and replays the available range;
WriteQuorum does not become an implicit All-follower wait.

Stopped-Primary reconciliation persists canonical Discover goals before
inspection and retains their exact native results in the existing
`replicaGoals` status journal. The result is canonical JSON in `nativeStatus`
(distinct from the typed snapshot field), validated against its accepted goal,
history, process/creation, configuration and primary fence. PC and CC read
quorums, deactivation/catch-up eligibility, highest-down evidence and the
positive-first-LSN lagger optimization determine the candidate. A requested
handoff target cannot override that decision; an accepted different target
remains pending. A changed nominee is inspected under its exact configuration
without incrementing the primary fence again. Current-only continuation also
keeps that fence. The source's achieved binding scope is not replaced by the
inspection's requested native epoch.

An excluded original member uses genuine Existing Open, not retained-secondary
initialization to obtain progress. The narrow journal-derived inspection
permission requires actual storage custody and old-owner exclusion; it neither
clears terminal/failure records nor permits serving, copying or voting.
Opened replies remain non-Ready in the selected native peer descriptions.
Original-member recovery separately achieves a configured Ready Primary with
application access closed. Full logical PC/CC membership and write quorums
remain intact while only actual up/Ready owners enter native peer vectors.
Configuration installation with insufficient peers is not quorum completion.
The excluded member achieves typed Idle preparation, then uses the existing
Build call, real context/copy/EOS/replication ACKs and BuildEnd before activation.
Joint catch-up, Ready history propagation, final current/end and accepted,
host-settled peer receipts precede serving and exact old-dependency linking.
The primary fence does not advance for those configuration-only steps.

A higher eligible Opened prefix can win over a lower warm nominee. Its actual
initial-Primary path still owes explicit epoch, application role and the
no-data-loss decision before Ready. A warm follower outside that source's
retained range takes an ordinary Close with a creation-owned, typed native
GetLSN rebuild cause. Successful local Cleanup is not physical exclusion:
the existing executable/supervisor must really exit and reap that owner before
Existing Open and the original-member Idle/Build path. This is not an
application fault, provider reset or permission to turn failed Close into
success. The old operation results remain unchanged, including lost controller
observations; only the exact accepted final native chain resolves their veto.

Pre-component ordinary23 frozen roots remain unchanged and report absent
facts; predecessor formats still reject without migration/reset. This is not
qualification of broader M3 recovery, interrupted handoff or PostgreSQL
recovery integration.

Generic publication binds resource/history, exact replica/storage/process/
creation, actual endpoint, epoch/primary/configuration and monotonic publication
generation. It contains no controller operation ID, Build attempt or ingress
grant. Factory context supplies read-only identity/publication and authenticated
peer discovery through the host-owned resolver, not a new callback into engines.
`Replicator`, `PrimaryReplicator` and `StateProvider` method sets are unchanged.

Primary access still needs paired application/engine roles, current epoch,
receipted configuration, selected current-only catch-up and quorum evidence.
Prepare, Build, scalar progress, idle diagnostics and input availability alone
do not activate serving. Secondary scope installation makes no primary-only
configuration callback. An unrelated Build does not replace a serving Primary's
access owner.

## Remote operations, retry and removal

Source remote state distinguishes **Absent → InBuild → Ready → InDrop → Dropped**;
an additional **Uncertain** observation denies reuse when current proof is lost.
Only successful ordinary Build plus current two-ended identity/scope yields
Ready. Exact endpoint/creation/process/storage/publication is revalidated at
callback return, replay/query, acknowledgement and voter admission, including
the actual configuration callback boundary after intervening awaits.
Target reopen invalidates Ready even if its replica ID/address stays the same.

Accepted active duplicates attach to the same retained ordinary call; losing
an HTTP/RPC response cannot discard it. Canonical changed input, stale goal/
epoch/instance and late completion cannot create renewed authority.
New nonlocal voters need exact accepted Ready; missing/substituted peer
descriptors, removed targets and remove-before-Build tombstones cannot bypass it.
Successful admission retires logical idle ownership without destroying admitted
replication. Later voter withdrawal never resurrects old Ready.

The builtin private carrier retains each binding/control effect in the
creation-owned transport task lifetime. An RPC observes that operation; it
does not own or restart it. An Active registry slot is not a completed Peer:
success requires its live carrier, ingress task and exact native ACK owner/
generation. Partial failures fence the binding and retain actual handles and
join errors. Exact retry reuses the result and rechecks present health and
scope. Recreating a process loses these in-memory roots; a journal receipt
cannot manufacture a warm receiver. Reform/Continue require a genuinely
completed native root, not a vacant provider-seeded secondary.

Ordinary Build nonce identity binds the canonical operation/digest, remote
generation and exact publications. Both ends read the existing authenticated
operation snapshots before activation. A typed original-member selection names
the exact retained inspection and achieved Idle preparation; logical quorum
membership is not an Active session or a copy receipt. The normal outside-
membership validator remains strict. Configured-source and copied-root
admission use the same journal, authenticated snapshots and actual native
owners; a publication or provenance nonce alone supplies no copy or vote.
Same-primary configuration changes retain an authorized actual root and ACK
generation; removed roots are fenced, not continued with an artificial epoch.

Ready removal on a still valid Open/Primary source calls ordinary Remove and
retires **local** idle ownership only. It is not proof of target receiver
drainage. InBuild cancellation uses a whole-creation Abort goal, never Remove
on an aborted allocation; target settlement is separate.
The built-in engine takes the session effect gate **before** the native
state/manager locks, matching authenticated ingress. Under those native locks,
it excludes both current and previous voting membership, even when the peer
also retains native idle membership, before fencing or removing anything.
Rejected voter Remove changes neither endpoint's session, native sender/build,
transport nor ACK owner. Legal idle removal still fences synchronously under
the same configuration-adoption lock and retains remote
retirement/join work even if its caller disappears.
An exact owned transfer still awaiting native Build may also be cancelled,
but only after the same native lock excludes current/previous voters.
The facade rechecks the peer's effect fence while selecting configuration
inside its native command actor, so an already validated queued callback
cannot readopt a legally removed peer while its metadata is still retained.
Acknowledging the exact retained settled Remove result is retryable after its
negative watermark is committed, including a lost reply or later controller
status conflict. It never repeats Remove or proves target containment.
Selection and Build replay remain fenced by tombstones and rejection watermarks.

Remote selections are bounded per creation/target. Settled terminal results may
compact only below a durable rejection watermark, after acceptance, retirement
and settlement dependencies resolve. Uncertain/active/unaccepted or
containment-pending records are not TTL-forgotten; exhaustion rejects before
effects. These are host-operation bounds, not M2 engine-session tombstones.

## Withdrawal, failures and restart

Incompatible primary/history/storage/process/creation or epoch scope cannot
silently replace publication. Compatible same-creation configuration updates
use ordinary callbacks; incompatible replacement requires withdrawal:

1. Unpublish and deny access before ordinary Abort.
2. Retain ordinary Close, application Close and host-call joins.
3. Acknowledge containment/release the provider writer only after actual required
   success, or independently verified process containment.

[Settlement](../../../kuberic-runtime/src/host/hosting.rs#L1079) retains sticky
errors and joins late factory allocations as well as registered children.
Failed preparation uses the same barrier. A child Close error remains a failed
public outcome even after other descendants join. Failed/lost/hung joining
keeps containment and conflicting storage reuse closed. Repeated observers
cannot repeat an ambiguous destructive callback or erase a cleanup failure.
Fresh creation after whole-creation Abort requires contained host/process
replacement, not clearing flags or resetting storage.

Matching-format restart preserves goals, selected-call uncertainty and application
bytes, but invalidates old process-bound publication. Fresh Open advertises fresh
DiscoveryPending; old interrupted Build is uncertain, never replayed as success.
Fresh recovery/callback evidence is required before achieved eligibility.
The ordinary reconciler retries the exact retained Discover identity while its
response or publication is outstanding, then reobserves before selecting more
work. It must not append a second Discover from a pre-submission snapshot.
Settled predecessor terminal goals remain retained but cannot block a newly
contained process's preparation or supply its activation receipt. Singleton
reopening selects fresh current-process catch-up and refreshes its session only
after actual achieved primary scope and granted access; this is not remote-peer
readmission or recovery-election policy.
The live secondary-reopen oracle uses authenticated ordinary snapshots, exact
retained goal outcomes and the current publication/storage/process/creation
and topology session, not legacy callback completions or advisory role strings.
The qualified crash-image fixture is not process-kill, power-loss or exhaustive
M3 recovery evidence.

## Fresh-deployment format

Host schema, control identity/envelopes and required
`KubericSet.spec.hostingFormat` are **23**, including generated CRD, shipped
sample and fixtures. The earlier M1/M2 format acceptances are historical, not
supported predecessor readers. Retained CRD goals/outcomes are
structurally typed and canonically validated. Old/missing/mixed formats and
legacy retained commands reject before authoritative mutation.

There is no old-state import, upgrade/downgrade reader, converter, mixed-version
fallback or reverse migration. Never delete, reset or reinitialize application
data or PGDATA to fit this format. Provision fresh compatible resources.
Matching-format SQLite transaction rollback-journal recovery remains distinct
from software downgrade; neither acknowledged data nor uncertain operations may
be discarded on restart.

## Verification and limits

Inspect the exact live package/features/profile/expression before each run:

```sh
cargo nextest list -p kuberic-controller --all-features --profile ordinary \
  -E 'test(/production_ordinary_/)'
cargo nextest run -p kuberic-controller --all-features --profile ordinary \
  -E 'test(/production_ordinary_/)'
cargo nextest list -p kuberic-runtime --lib --all-features --profile ordinary \
  -E 'test(/ordinary_goal/)'
cargo nextest run -p kuberic-runtime --lib --all-features --profile ordinary \
  -E 'test(/ordinary_goal/)'
```

These executed selections cover real ordinary callbacks/listeners/nonempty
fixture copy, held multi-replica recovery/discovery, lost replies, stale actions,
receipt gaps, exact Ready/separate admission, failed preparation, retained
engine/service/host joins, writer fencing, tombstones/bounds and matching-format
uncertainty. [Testing](testing.md#ordinary-hosting-m1) records validation scope.

Actual built-in/application and independent PG witnesses execute copy, ACK,
native SQL/WAL/replay and real joining. Former unavailable wrappers and
grant-era fixtures have been converted; shared companion/grant/control fields
are removed. Their assertions now live at ordinary host or concrete engine
owners, rather than disappearing with the old API.
Historical broad bodies remain enabled and red at their actual later limits.
The [evidence boundary](testing.md#m2-accepted-evidence-and-remaining-boundaries)
records aggregate infrastructure termination, complete native complements,
changed-primary handoff and replacement/election diagnostics without crediting
unexecuted suffixes. Duplicate Discover, incomplete retained-member copy and
stale predecessor-terminal Build veto were M2 defects and are now repaired,
not deferred as M3.

Automatic admitted contraction, planned primary replacement and retained-history
election/reformation are explicitly unavailable or waiting for fresh evidence.
M3 must qualify the full crash/failover/replacement matrix.
PG durable metadata, WAL/backup/slots/lineage and physical SQL/process obligations
remain; [metadata removal](../postgres/stateless-metadata-prerequisites.md) is
separately deferred.

Bearer authentication is not TLS or hostile-administrator/orphan protection.
Full owned live bootstrap passed on the exact recovery image source recorded
in [testing](testing.md#m2-accepted-evidence-and-remaining-boundaries); it predates
the final terminal-owner controller repair and is not its live qualification.
No native executable differential, power-loss, downstream
MySQL, broad settings/security or production-readiness certification is claimed.
Existing [source provenance and notices](sf-v1-port-provenance.md) remain.

### Bounded M2 retained-member recovery repair

The built-in engine now distinguishes recovery of an admitted secondary from
fresh idle Build. With an unchanged available Primary, current configuration,
epoch and history, a fresh process on the same established storage reserves a
new authenticated private session while DiscoveryPending. The source verifies
its retained completed predecessor, fences and joins the old carrier/effect/ACK
owners, and replaces the native sender without changing membership or inheriting
ACK credit. Actual provider copy/context and replication-boundary acknowledgements
must finish before ProgramEpoch and the ordinary preparation outcome succeed.
Transfer traffic remains nonvoting until the source's ordinary configuration
callback selects the completed sender. The incomplete-copy guard remains intact.

KV supplies the complete retained committed prefix as copy context. The source
checks that prefix against its own history before returning a nonregressing
snapshot; the application committer durably installs it before opening the
replication consumer. Incomplete, corrupt or conflicting inputs fail closed,
without resetting application data. Retirement retains uncertainty and receiver
reservation; late copy cannot install into a successor.

Before spawning recovery work, the source reserves an exact attempt in its
existing creation-bounded session owner. Only one nonce may replace a given
target/predecessor; exact duplicates attach to its retained immutable result.
Pending and failed attempts consume that budget too. Exhaustion rejects before
allocating results or spawning work, without TTL-forgetting uncertainty.
After discovery awaits, the captured reservation and source publication must
still match. Predecessor removal compares the complete binding under the peer
map lock; a stale recovery cannot take a successor's carrier or ACK owner.

Recovery completion observers await that exact retained source result instead
of treating unfinished work as QueueFull and repeatedly serializing authenticated
poll requests. Waiting observers remain bounded by the existing control budget,
with capacity left for activation and ACK progress. After the result settles,
the source still revalidates the current binding and actual completion; neither
waiting nor an old successful result grants admission after retirement.

The outgoing carrier's first stream binding and tracked connection child are
registered under the exact session effect gate before native Build can send
StartCopy. An unpolled connection owner therefore retains that first frame
instead of losing it until the sender's retry timer. Retirement still joins
the actual connection child, including work held in publication validation;
binding a stream does not bypass authenticated peer admission or grant ACK
credit.

The controller accepts the exact fresh preparation, then obtains ordinary source
configuration/catch-up and target reconfiguration outcomes before updating the
topology session. The source callback validates its retained completed recovery
session, not descriptor numbers alone. Fresh voter additions still require exact
two-ended Ready, configuration receipts and separate access admission.

`TestCluster::request_restart` observes a real fresh, unadmitted process;
`restart` additionally settles the configured membership cardinality. Native KV
recovery tests preserve acknowledged data, closed recovery access, fresh ongoing
replication and post-restart quorum writes. This bounded repair is part of
accepted M2 implementation evidence, not general M3 election, primary
replacement, contraction or handoff qualification.

An achieved predecessor Close remains in retained history after restart, but
does not veto a fresh idle Build for the admitted new process/creation. Both
Build submission and later scheduling use the same exact live storage/process/
creation terminal check. A current owner's Abort or Close still blocks Build,
including pending, failed or uncertain settlement; no terminal history is
discarded and exact two-ended Ready and separate voter/access admission remain
required. Production-controller regressions observe the actual native target
copy before acceptance, and KV growth checks acknowledged `retained`/`later`
values plus continuing replication before requesting any replacement. The
historical scenario's subsequent secondary replacement remains a separate M3
diagnostic, not a fresh-growth acceptance waiver.
