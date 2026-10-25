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

[Snapshots/discovery](../../../kuberic-runtime/src/host/ordinary_goal_service.rs#L28)
do not invoke engine progress, capability or diagnostic callbacks. Ordinary
progress/capability are explicitly **NotQueried** at this checkpoint; they are
not zero or a recovery certificate. Retained-history election cannot infer safe
progress from such observations.

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

Ready removal on a still valid Open/Primary source calls ordinary Remove and
retires **local** idle ownership only. It is not proof of target receiver
drainage. InBuild cancellation uses a whole-creation Abort goal, never Remove
on an aborted allocation; target settlement is separate.
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
`KubericSet.spec.hostingFormat` are **16**, including generated CRD, shipped
sample and fixtures. M1's original format-15 acceptance is historical, not a
supported reader. Retained CRD goals/outcomes are
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
