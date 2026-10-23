# Classic V1: Current Semantics

The [engine-neutral hosting contract](engine-neutral-hosting.md) supersedes
older host-only anchors below for common admission, retained authority,
observation separation and public terminal outcomes. Its implementation is
anchored through `91a2a93c3c7cf9477adb3a0d27e674b99f741a8a`; older queue/
provider/component anchors remain historical evidence, not rerun claims.

This is the authoritative behavioral reference for the selected classic V1
runtime. Selected-host Close/access evidence is pinned to Kuberic
`69b1cfe3c85d296463da0def3e80eaca0ddcd0a7` (Close references retain their
explicit `aad23335c219a576d69133cee883bbd63e40b68a` anchors); unchanged component references
retain their explicit `e2a47b6c631c974aa0135e81c3a7e3a439980ded` anchors.
Native comparisons use Service Fabric
`3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d`.

Bounded custom-controller formation evidence is separately anchored to Kuberic
`366b362ce4e1c9fdc6aaf0fe64e6be78931e96a3` and the
[supported companion contract](replicator-boundary.md#custom-controller-integration).
Current authority/ownership contracts use the linked source and witnesses below.
The PostgreSQL controller witnesses and runtime negative fixtures do
not change the older pinned built-in/component evidence below.

**Evidence is bounded:** targeted Rust selected-host regressions were executed
for the Close/access changes. Inspected source and other existing test bodies
are not additional runs, native binary differential, power-loss proof,
exhaustive compatibility certification or production-readiness claim. The
[evidence/disposition ledger](sf-v1-port-ledger.md) distinguishes supported,
explicitly unsupported/different, unverified and deferred work. See the
[pending-items plan](sf-v1-semantic-alignment-plan.md) for actual remaining
alignment/qualification and [provenance](sf-v1-port-provenance.md) for source
ancestry and notices. [Testing](testing.md) owns commands, not this page.

## Selection and ownership

The sole built-in `DefaultReplicatorFactory` calls `v1::create_production`,
composing private queue, sender/session/ACK, primary, secondary, copy, role and
recovery owners ([factory:1138–1156][factory],
[composition:127–227][composition]). There is no old-engine fallback in that
factory. This does not prohibit application-owned custom factories: PostgreSQL
has one that rejects a supplied provider and exposes no StateReplicator
([PG service:246–292][pg-factory]).

Applications use the protected `Replicator`, `PrimaryReplicator` and
`StateProvider` contracts, not private engine types. `Lsn = i64`,
`OperationData = Bytes` and boxed `Send` result streams remain the
representations ([application:14–32][types]). The normal-feature external
provider fixture and public journey need neither a runtime testing helper nor
an extra durable-progress hook ([fixture manifest:9–17][fixture-manifest],
[journey:1861–1939][external]). Optional example persistence helpers are not
engine prerequisites.

This is a source-guided Rust responsibility port, not native COM ABI or wire
compatibility, an RA/FM/placement port, or native process hosting. The tonic
carrier/host/controller adaptations are recorded in
[provenance](sf-v1-port-provenance.md#execution-and-platform-substitutions).

## Admission, assignment and application commitment

`begin_replicate` returns an admitted `lsn` and separate quorum-completion
future; convenience `replicate` awaits that same future
([production:473–491][assignment]). Failed queue admission does not consume
the next sequence ([queue manager:227–250][enqueue]). Native completion is
not local service commit, SQL publication or client success.

The selected service adapter samples live host write access under the **same
short gate** used by Revoke/replacement fencing, and holds it through sequence
assignment ([adapter:421–448][admission],
[host Revoke:374–380][revoke]). The primary releases the lease after enqueue,
before sends or pressure/provider effects ([primary:622–654][lease]). A call
approved before the fence assigns within the refreshed final configuration's
cut, or rejects; this is not a drain of service commitment or arbitrary provider
callbacks. The actual host-gate regression and public post-fence raw admission
assertion support this claim ([admission test:11–145][admission-test],
[public test:4396–4422][swap-fence]). The historical SF-02 “KV bypasses write
denial” label is not a demonstrated current bypass merely because the KV HTTP
method does not itself call async partition status.

Services retain accepted work independently of a client's waiter. They own
durable preparation, ordered local commitment, committed read visibility and
client response; observer cancellation is not rollback:

| Service | Selected commitment/publication boundary | Existing evidence and limit |
|---|---|---|
| KVStore2 | Actual assignment → durable prepare → retained FIFO waits quorum → local commit → reply; reads use the committed view | [KV:195–240,324–376][kv-commit]; [held-commit/read test:554–611][kv-test] keeps speculative state/client success hidden even while native reads are Granted. Secondary durable acceptance precedes ACK ([service:416–490][kv-secondary]). |
| SQLite | Retained SQL/VFS work checks access, assigns, prepares frames, waits quorum, commits frame history, then permits WAL publication; successful SQL/publication precedes response | [SQLite:218–241][sql-response], [272–345][sql-commit]; [real-WAL test:1731–1801][sql-test] separates local frame commit, publication and client result. File restart evidence is not kernel power-loss proof. |

Partition read status is an admission policy, **not per-transaction isolation**.
KV checks it before reading its committed view; stable-secondary normal reads
remain denied despite advanced local data ([KV read:182–193][kv-read],
[read test:569–590][kv-test]). Do not reintroduce the obsolete provider
`applied == committed` rule as the meaning of native access.

## Shared streams, ACK and terminal states

Received operations expose kind, LSN, epoch, data/EOS and parameterless
`acknowledge(&self)`; dropping one is neither ACK nor rollback
([public stream:19–51][stream]). The production bridge delegates to the native
operation callback, not a progress-bearing service oracle
([production:341–378][stream-bridge]).

- Getters share consuming dispatch queues: no rewind or independent replay.
  Empty **open** queues wait; normal copy completion eventually gives `None`.
  Configured explicit EOS is a distinct operation whose ACK can hold Build
  ([shared stream:112–154][stream-owner],
  [shared-getter test:774–829][stream-test],
  [public EOS test:1744–1796][eos-test]).
- Production derives persisted/volatile mode from the public lifecycle
  `StatePersistence` binding. Persisted replication/copy requires explicit
  service ACK; selected volatile replication can complete at ordered dispatch,
  before service dequeue/ACK. Core tests also cover volatile required-ACK,
  but it is not a public selectable setting
  ([production:121–158][receiver-policy],
  [mode tests:121–172][mode-test]). Persisted public evidence separates receive
  progress from held completion until ACK ([public:1696–1740][ack-test]).
- A retired/aborted stream reports typed `RuntimeError::Closed`, not successful
  copy EOF. Sticky abort is an error once per nonterminated/new reader, then
  EOF; already graceful EOF stays terminal. This is not “every later poll
  errors,” nor a promise that every late ACK fails
  ([facade:132–147][closed], [stream owner:112–154][stream-owner],
  [retained-stream test:958–971][retired-test]).
- Duplicate detection uses **LSN/queue position, not replayed-byte identity**.
  The existing root/original epoch is retained and not redelivered. Core tests
  change epoch rather than payload; the absence of byte comparison is visible
  in queue code ([queue:348–359][duplicate],
  [receiver test:203–222][duplicate-test]). Conflicting application history is
  a separate service responsibility.

## Copy and Build

Persisted copy gets context from the **target** and passes the incoming context
to source `get_copy_state(start - 1, context)`; volatile mode omits context
acquisition ([establishment:473–478][context-mode],
[738–757][context-source]). Distinct nonempty target bytes and an empty item
reach the source in an actual-component test
([test:1013–1091][context-test]). Separately, an executed helper-free
[external public Build][public-context] consumes exactly
`[b"target/context", empty, b"target/trailer"]` through EOF, with no source
context acquisition. The standalone runtime feature graph contains `host`,
not `testing`; its workspace wrapper is testing-unified evidence. Real
pre-Build service commits supply a nonzero copy boundary and distinct
application values installed into fresh checked target storage. The matched
Build completion is successful, the exact remote Build is Ready, and both
public Close/host waits/provider drains succeed. The target remains idle
outside voters with serving reads denied: copied persistence is not admission
or catch-up certification. The [default empty policy][context-default] and
original [empty-context public journey][external] remain unchanged; the
[historical empty-only fixture][empty-context] retains its original pin.
See [reproduction and evidence limits](testing.md#public-target-origin-copy-context).

At actual enumeration EOF, CopySender samples **latest issued**, not merely
committed, replication progress. Final-copy application and replication apply
through that target remain separate completion gates. Pending retargeting only
raises the target while its gate remains pending
([sender:194–293][copy-target], [443–459][copy-gates]).
Quiet empty/nonzero snapshots and later suffixes have current public witnesses
([public:1661–1692][quiet], [1800–1840][suffix]); no invented boundary record,
durable receipt envelope or engine commit oracle is required.

Copy and replication are consumed in sequence by the external provider:
copy EOF before replication consumption. It acquires the replication handle
earlier; this does not prove every getter ordering or arbitrary in-place
stream/build reset ([fixture:448–505][external-streams]).

## Quorum, catch-up and progress

Normal write completion obeys separate previous/current configuration (PC/CC)
quorum requirements. Receive ACKs constrain reclamation; apply ACKs constrain
commitment, while idle retention is distinct. Local-only CC/W1 can coexist with
former-primary-only PC/W1: that PC remote's **apply**, not receive, still
constrains normal completion. W1 with remote CC members rejects
([manager:846–938][quorum], [primary:480–485][w1]).

Catch-up is not a fresh union-membership or independent PC+CC proof. With
`T = latest issued`, `A = all-current applied`, `Q = current quorum applied`,
`B = captured configuration boundary`, `M = minimum live marked apply`:

- **All:** `A >= T`; follows new writes while pending.
- **WriteQuorum:** `Q >= B`, plus `M >= Q` when successors are marked.
- Configuration updates capture/raise `B`; every ordinary write/wait does not.

These are the actual selected predicates
([manager:975–1024][catchup], [queue-manager:394–404][capture]),
with [native:43–105][native-catchup] ancestry and
[actual tests:641–719][catchup-test]. The
[protected catch-up contract][catchup-doc] describes these selected V1
conditions separately from ordinary PC/CC completion and provider commitment.

### Rejection and fault scope

A dispensable peer's ordinary connection/Exchange rejection supplies no apply
progress and does not itself fail the primary's quorum wait. A remote idle
copy/context error fails that peer's Build, not the primary's local provider.
On an established active session a copy-error header is not a generic
replication NACK; valid replication progress remains separately meaningful.
These distinctions follow [native send handling:753–790][native-send-errors]
and [ACK handling:633–740][native-ack-errors], with actual selected
[owner vectors:18–155][rejection-owner].

The public [both-order vectors:57–320][rejection-public] exercise a real
configuration-mismatched C listener returning PermissionDenied while P/B
retain valid authority. A rejection-only observer forwards the exact source
binding and returns C's status unchanged; source retry entry establishes
consumption before releasing B in rejection-first order. Healthy-first holds
C's initial dial until genuine B apply, marked WriteQuorum catch-up and service
commit complete. Both orders use nonempty work, a captured issued boundary,
service-owned persistence/ACK and another successful write after rejection.
An opt-in [same-native-wait handshake:590–625][pending-native-wait] crosses
an ordered query on the selected facade, asserts the retained native future
pending before release, and rechecks it after rejection-first consumption.
Outer host-task incompletion alone is not this native-pending witness.
This is selected Rust host/carrier evidence, not a native transport run.

Dispensability is operation-specific: missing required PC/CC apply still holds
normal replication; All requires all current members; a marked member behind
live Q holds WriteQuorum catch-up. These conditions do not manufacture an
immediate remote-error result or waive progress
([actual-owner controls:93–155][rejection-owner]).
Primary-local GetCopyState/EndGetNext faults are separate: new assignments
and new catch-up entries reject the latched fault before queue/wait mutation
([manager:793–805,1028–1045][local-fault-entry],
[owner:157–230][rejection-owner], [public:333–392][rejection-public]).
This does not promise synchronous fault completion of every already-pending
wait, or revoke a previously completed success. Invalid live-owner protocol,
local representation/authority and session/epoch failures retain their own
drop/fault policies; they are not blanket dispensable errors
([selected ingress:355–411](../../../kuberic-runtime/src/replicator/v1/service_adapter/production_transport.rs#L355-L411)).

| Query | Meaning, not a durability alias | Evidence |
|---|---|---|
| Primary current | Queue `commit_head - 1`, native quorum progress; can precede local service commit | [queue-manager:322–331][primary-progress]; [public held-commit witness:16–71][primary-progress-test] |
| Primary capability | First **committed** queue entry (`complete_head` unless equal to `commit_head`), or zero; not every retained entry | [queue-manager:330–331][primary-progress]; [queue:144–157][heads] |
| Secondary current | Contiguous received/queue-committed cursor, possibly ahead of service ACK | [receiver:369–385][secondary-progress]; [public witness:76–139][secondary-progress-test] |
| Secondary capability | First retained **completed** queue entry, or zero | [receiver:379–385][secondary-progress]; [queue:144–149][heads] |
| Incomplete copy | Explicit protected query can return NotReady and report a fault; routine telemetry checks readiness non-destructively first | [secondary:340–374][not-ready]; [host:80–101][telemetry] |
| Initially Opened | Lazy provider progress; successful negative values normalize to zero; capability zero | [adapter:237–262][opened]; [Open test:222–248][open-test] |

Reports retain independent Value/NotReady/Failed and intentionally skipped
NotQueried outcomes, not an ordered applied/committed pair. The public decorator
matrix covers RPC, fsynced/reopened **observation snapshots** and controller
eligibility, not every native fault or the agent recovery journal
([matrix:991–1085][query-matrix], [1131–1168][query-snapshots]).

Primary epoch provider callbacks use issued tail, including duplicate epochs.
Secondary direct epoch updates and stream/provider barriers are different paths
([manager:292–302][epoch], [role:399–435][secondary-epoch]). The
[provider epoch contract][epoch-doc] preserves the supplied boundary without
certifying quorum commitment or provider durability.
Changed data-loss recovery can reset higher/lower/equal/empty progress and next
assignment; it is not globally monotone across history/session reset
([data-loss test:233–252][data-loss]).
`INVALID_LSN` is an input sentinel, not empty history or permission to reuse
progress across identities ([description:153–163][sentinel]).

## Host access, lifecycle and retirement

The selected public lifecycle program calls replicator before application;
Revoke is access-only, not synthetic `change_role(None)`
([program:14–44][program]). Stable-secondary access is NotPrimary
([host selector:187–195][stable-secondary]).

Selected production now publishes live operation-owned stages, not the private
host's full selector. Paired-role/configuration/catch-up evidence and matching
completion determine access ([selected policy:209–300][selected-policy],
[activation:721–766][selected-activation]):

| Reachable stage | Read | Write |
|---|---|---|
| Same-primary joint install/catch-up, including caught-up joint completion | Granted | C and P; otherwise NoWriteQuorum |
| Matching current-only activation | Granted | C; otherwise NoWriteQuorum |
| Promotion before paired roles and catch-up | ReconfigurationPending | ReconfigurationPending |
| Promotion caught up, before activation | Granted | ReconfigurationPending |
| Demotion pre-Revoke catch-up | Granted | C and P; otherwise NoWriteQuorum |
| Demotion post-Revoke final wait | Granted | ReconfigurationPending |
| Demotion final wait returned, role transition and completion | NotPrimary | NotPrimary |

C requires current electorate minimum size and available majority; P requires
previous available majority when a previous set is supplied. Required
`is_up`/`is_ready` descriptor facts are explicit control-plane observations,
not live peer-health or commitment certification. No previous set makes P
vacuous, not an invented old electorate. Joint completion retains P;
current-only activation removes it. Actual write completion still needs native
transport/quorum. Split controller Role/Epoch/CatchUp and lifecycle-recipe
promotion both require catch-up; plain Configuration cannot bypass it.

Stage reservations, invalidation and final publication share the real
admission gate. Stale/cancelled/superseded owners cannot replace a newer owner;
fault, terminal, creation and history/storage denials override grants.
Accepted RPC publication survives reply-observer loss. Demotion denies reads
immediately after final wait, before a fallible outcome receipt. These are the
reachable native branches ([selector:288–381][native-stage-selector]), not its
unreachable demotion CatchupCompleted table row. Deterministic real-public-host
barriers attempt service committed reads and raw assignment across these cuts
([stage witnesses:144–2159][selected-tests]); private selector tests alone are
not the evidence.

Same-primary grants require matching previous/current primary context, not just
old paired-role receipts. A demoted host can later promote again: compatible
final-demotion evidence survives interrupted/superseded reservations without
carrying serving grants. Actual native role/epoch observation, fresh application
role, configuration, catch-up and activation remain required. Nonactivating
configuration or uninvoked role reservations cannot consume that evidence
([same-host/lineage witnesses:919–1545][reconciliation-tests]).
Controller unavailable current/capability observations produce explicit
not-ready descriptors rather than leave old availability installed
([producer/selected witness:484–599][producer-tests]); restored admission is
distinct from a completed transported write.

**Outer Close** records the child identity/error, aborts both children and skips
later graceful callbacks after a child failure. Successful actual terminal
cleanup yields positive containment, but the current common executor retains a
**failed public Close result**. This deliberately differs from native's
success-shaped outer outcome and supersedes the older selected-host disposition
([current program](../../../kuberic-runtime/src/host/public_lifecycle_program.rs#L587),
[current terminal owner](../../../kuberic-runtime/src/host/public_lifecycle_terminal.rs#L41)).
The local abort-both/skip-later-graceful sequence is also intentional. Durable
`CloseFailure` outcomes retain child/operation diagnostics; cleanup failure
remains failed and cannot publish positive containment. Actual controller-
dispatched V1 and PostgreSQL terminal witnesses are described in the
[bounded hosting reference](engine-neutral-hosting.md#compatibility-and-qualification).

An unrecorded interrupted destructive child callback is not retried: recovery
attaches an Abort/drain/witness owner while retaining Ambiguous/Cancelled
disposition ([recovery:79–115][selected-recovery]). Recorded Cleanup is not a
new process-local join. Shutdown joins owners discovered by final recovery.
Selected failed-replicator/application, interruption and retained-provider
barriers exercise these distinctions ([public:3114–3505][selected-close-tests]).
Some recovery cuts are deliberately seeded records; they are not power-loss
or arbitrary process-crash qualification. Arbitrary application tasks have no
generic join API; the failing-application fixture joins its own work.

Abort requests containment; caller cancellation does not prove task drainage.
Direct facade Close can complete while arbitrary Build provider work remains;
the separate join can still be pending
([facade test:351–387][close-join]). KV trait Close retains consumer joins/errors
([service:200–218][kv-close], [test:478–532][kv-close-test]).

Native idle Remove changes membership/retention and stages copy cancellation
and sender close, **not arbitrary provider joining or remote storage deletion**
([manager:765–789][remove]). Removed callbacks may remain rooted; late completion
cannot replace a new Build under an old ID
([primary test:1234–1274][remove-test]).

## PostgreSQL custom path

PostgreSQL uses its own no-provider/no-StateReplicator bundle with conservative
specific-quorum capability false ([service:246–292][pg-factory]).

The production factory attaches its own creation-bound custom companion, not a
built-in runtime. It validates exact host authority at native ingress, gateway/
backup boundaries and completion; source inspection independently validates the
live source operation. Actual WAL/lineage/replay and database evidence remain
application-owned. Exact live native replay revalidates retained/physical
history and returns the matching completion without repeating copy mutation.
Revoked or stale authority cannot reuse that historical completion as ingress.

Overlapping hosted native requests attach to one retained exact attempt.
Queued/replay observers never acquire process cleanup authority; the serialized
owner revalidates captured cancellation and live authority before slot/PGDATA/
helper effects. A delayed rejected observer cannot stop or rewrite the later
admitted peer. Direct-native caller cancellation remains a separate explicit
operation cut, not a hosted accepted-operation response observer.

Graceful Close interrupts pending work but retains a still-safe bounded final
durable observation before lifetime cancellation; Abort does not invent that
observation. Both paths retain exhaustive joins/errors. A completed close is
idempotent and does not repeat effects against a later storage owner.

The engine retains last successfully installed native membership and actual
completed built-idle ownership separately from current transport/readiness.
Normal same-history epoch advancement may drain transport without erasing the
truthful predecessor needed for joint catch-up; it does not install the next
configuration or preserve revoked copy authority. Secondary host configuration
precedes primary native Peer admission and is not itself native peer completion.

For initial authority only, the controller can select a deterministic singleton
when every eligible replica has exact successful **New-Open completion
provenance**, None role, no prior authority/installed state and both explicit
`Value(INVALID_LSN)` query results. Current report Open mode is not freshness.
This is local initialization policy, not a retained-history election or a zero
progress claim; failed/unavailable queries do not qualify.

The [actual controller cases](../../../examples/postgres/src/public_controller_tests.rs)
qualify nonempty singleton → Build → separate joint/current-only admission,
live replay and binding rejection, source/target invalidation before delayed
completion, synchronous SQL/replay and retained-socket rejection after completed
native fencing. Cleanup errors propagate while real owners are joined. Contract
stale-revoke callbacks are explicitly negative component checks, not substituted
controller positives. Automatic reformation after arbitrary failed custom Build,
external MySQL requalification and native executable differential remain outside
this evidence.

Current source anchors are the
[PG companion/authority:547–864](../../../examples/postgres/src/public_v1.rs#L547-L864),
[epoch handling:1570–1604](../../../examples/postgres/src/public_v1.rs#L1570-L1604),
[Build/replay:2365–2698](../../../examples/postgres/src/public_v1.rs#L2365-L2698),
[graceful Close:1273–1323](../../../examples/postgres/src/public_v1.rs#L1273-L1323),
[fresh authority gate:1084–1095,1199–1223](../../../kuberic-controller/src/evaluator/public_v1_topology.rs#L1084-L1095)
and [fresh provenance predicate](../../../kuberic-controller/src/evaluator/public_v1_topology.rs#L1199-L1223).

| Boundary | Current behavior and evidence | Limit |
|---|---|---|
| SQL access | Samples live partition status before cache reuse; primary SQL requires read+write Granted; an otherwise eligible secondary needs receive **and replay** at its source boundary ([PG:626–731][pg-access], [tests:3–158][pg-access-test]) | Owned periodic reconciliation is not per-statement mediation or instantaneous revocation ([PG:412–421][pg-tick]). Isolated secondary test status is not host-granted stable-secondary access. |
| Catch-up | **Marked-successor WriteQuorum explicitly rejects before waiting**; ordinary unmarked WriteQuorum and All are supported ([PG:1476–1504][pg-catchup], [tests:253–368][pg-catchup-test]) | Unsupported is not implemented/ignored. Capability-false swap chooses All ([controller:445–470][fallback], [public wait assertions:4396–4412][fallback-test]). |
| Idle Remove | Rejects current/previous voters; retires matching idle grants and reconciles managed physical slots ([PG:1919–1952][pg-remove], [slots:212–258][pg-slots], [tests:607–688][pg-remove-test]) | Same PID/healthy SQL and slot absence are bounded evidence, not a disk-growth SLA. |
| Stopped progress | Orderly drain snapshots primary certificate versus secondary receive/replay before stopping ([PG:567–617][pg-stop], [1507–1525][pg-progress], [test:163–249][pg-stop-test]) | Not arbitrary-survivor ranking, unobserved SQL racing shutdown, an Abort snapshot or power-loss proof. |
| Abort | Retained cleanup owns grant cancellation/gateway drain ([PG:500–516][pg-abort], [tests:940–1050][pg-abort-test]) | Existing process/socket/listener observations without prior Close/Drop are not synchronous arbitrary-work join or production latency guarantees. |

## Production policies and security

These are **selected fixed policies**, not complete native public defaults or
settings parity ([production:40–66][policies], [121–190][production-policy]):

| Policy | Selected value/boundary |
|---|---|
| Replication queue / sender | Initial 64, maximum 8192; queue byte cap 256 MiB; no retained completed-history budget; sender retry 5s |
| ACK / Close | Batch 15ms, max pending 1024; pending-quorum Close prewait 5s |
| ACK/EOS | Persisted/volatile derived from lifecycle; explicit ACK forced by persistence; EOS enabled in selected receivers |
| Data | Raw single buffer, one segment, payload cap 52,428,800 bytes ([selected buffer limits:35–43][buffer-limits]); private lossless segmented codec is not a public selectable policy |
| Public settings | Address-only. Effective endpoint replaces initial port-zero input; unchanged effective endpoint accepted, rebinding rejects ([settings:275–283][settings], [390–438][effective], [test:451–502][effective-test]) |

Native public defaults include different queue/ACK/EOS/Close choices; native
settings retrieval/update/credential semantics have no full public equivalent
here ([native defaults:136–207][native-defaults],
[native settings:762–808][native-settings]). Existing persistence declaration
and basic partition load/fault reporting are present, not wholly missing
([partition surface:1036–1064][partition]). Broader policy/settings/report
capability expansion remains [deferred inside the original target](sf-v1-semantic-alignment-plan.md#deferred-broad-p7--still-in-the-original-target).

The production transport **does authenticate bearer credentials but does not
provide TLS**. It requires `http` advertisements and starts an unencrypted
server ([transport:177–186][http], [217–227][server]); peer binding checks the
host agent bearer token ([728–766][bearer]), as do control requests
([service:1212–1224][control-auth]).
Protect tokens, namespace resources and the deployment network independently.
Lack of TLS is an ordinary deployment-security constraint, not merely a rare
optional extension. There is no public SF credential-settings/update API whose
acceptance would imply enforcement ([settings:275–283][settings]).

Every delivery revalidates current host journal authority/epoch; process,
storage, history, address and Build checks are independent
([transport:365–400][dispatch], [authority:148–226][authority]).
Authenticated missing-session **ACKs only** may be dropped after retirement;
other dispatch errors remain failures ([ingress tests:226–370][ingress-test]).
RPC acceptance/reconnect does not manufacture an ACK.

Public protocol 13 and agent schema 14 are breaking formats, not native wire
versions ([protocol:13][protocol], [schema:19–21][schema]).
Protocol 13 requires descriptor availability fields and includes them in
canonical operation identity ([descriptor:548–562][availability],
[encoder:3118–3133][availability-digest]). Protocol-12 identity and old
descriptor-bearing persisted JSON without those fields reject; recreate that
unstable state rather than infer available peers or mixed-version migration.
Do not infer mixed-version, migration, exhaustive deployment-security or
production-readiness support from the selected Rust journeys.

[factory]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/kuberic-runtime/src/replicator/mod.rs#L1138-L1156
[composition]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/kuberic-runtime/src/replicator/v1/service_adapter/production.rs#L127-L227
[pg-factory]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/examples/postgres/src/service.rs#L246-L292
[types]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/kuberic-runtime/src/application.rs#L14-L32
[fixture-manifest]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/scripts/fixtures/classic-v1-provider/Cargo.toml#L9-L17
[external]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/kuberic-runtime/tests/classic_v1_production.rs#L1861-L1939
[assignment]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/kuberic-runtime/src/replicator/v1/service_adapter/production.rs#L473-L491
[enqueue]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/kuberic-runtime/src/replicator/v1/replication_queue_manager.rs#L227-L250
[admission]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/kuberic-runtime/src/replicator/v1/service_adapter.rs#L421-L448
[revoke]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/kuberic-runtime/src/host/hosting.rs#L374-L380
[lease]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/kuberic-runtime/src/replicator/v1/primary.rs#L622-L654
[admission-test]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/kuberic-runtime/src/replicator/v1/service_adapter/tests/host/admission.rs#L11-L145
[swap-fence]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/kuberic-runtime/tests/public_v1_recovery.rs#L4396-L4422
[kv-commit]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/examples/kvstore2/src/v1.rs#L195-L376
[kv-test]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/examples/kvstore2/tests/read_fencing.rs#L554-L611
[kv-secondary]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/examples/kvstore2/src/service.rs#L416-L490
[sql-response]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/examples/sqlite/src/v1.rs#L218-L241
[sql-commit]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/examples/sqlite/src/v1.rs#L272-L345
[sql-test]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/kuberic-runtime/tests/classic_v1_production.rs#L1731-L1801
[kv-read]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/examples/kvstore2/src/service.rs#L182-L193
[stream]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/kuberic-runtime/src/replicator/stream.rs#L19-L51
[stream-bridge]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/kuberic-runtime/src/replicator/v1/service_adapter/production.rs#L341-L378
[stream-owner]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/kuberic-runtime/src/replicator/v1/operation_stream.rs#L112-L154
[stream-test]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/kuberic-runtime/src/replicator/v1/service_adapter/tests.rs#L774-L829
[eos-test]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/kuberic-runtime/tests/public_v1_recovery.rs#L1744-L1796
[receiver-policy]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/kuberic-runtime/src/replicator/v1/service_adapter/production.rs#L121-L158
[mode-test]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/kuberic-runtime/src/replicator/v1/secondary_receiver/tests.rs#L121-L172
[ack-test]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/kuberic-runtime/tests/public_v1_recovery.rs#L1696-L1740
[closed]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/kuberic-runtime/src/replicator/v1/service_adapter/facade.rs#L132-L147
[retired-test]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/kuberic-runtime/src/replicator/v1/service_adapter/tests.rs#L958-L971
[duplicate]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/kuberic-runtime/src/replicator/v1/operation_queue.rs#L348-L359
[duplicate-test]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/kuberic-runtime/src/replicator/v1/secondary_receiver/tests.rs#L203-L222
[context-mode]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/kuberic-runtime/src/replicator/v1/copy_establishment.rs#L473-L478
[context-source]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/kuberic-runtime/src/replicator/v1/copy_establishment.rs#L738-L757
[context-test]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/kuberic-runtime/src/replicator/v1/copy_establishment/tests.rs#L1013-L1091
[empty-context]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/scripts/fixtures/classic-v1-provider/src/lib.rs#L196-L207
[copy-target]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/kuberic-runtime/src/replicator/v1/copy_sender.rs#L194-L293
[copy-gates]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/kuberic-runtime/src/replicator/v1/copy_sender.rs#L443-L459
[quiet]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/kuberic-runtime/tests/public_v1_recovery.rs#L1661-L1692
[suffix]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/kuberic-runtime/tests/public_v1_recovery.rs#L1800-L1840
[external-streams]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/scripts/fixtures/classic-v1-provider/src/lib.rs#L448-L505
[quorum]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/kuberic-runtime/src/replicator/v1/replica_manager.rs#L846-L938
[w1]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/kuberic-runtime/src/replicator/v1/primary.rs#L480-L485
[catchup]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/kuberic-runtime/src/replicator/v1/replica_manager.rs#L975-L1024
[capture]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/kuberic-runtime/src/replicator/v1/replication_queue_manager.rs#L394-L404
[native-catchup]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Replication/PrimaryReplicator.CatchupAsyncOperation.cpp#L43-L105
[catchup-test]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/kuberic-runtime/src/replicator/v1/replica_manager/tests.rs#L641-L719
[primary-progress]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/kuberic-runtime/src/replicator/v1/replication_queue_manager.rs#L322-L331
[primary-progress-test]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/kuberic-runtime/tests/support/read_fencing.rs#L16-L71
[heads]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/kuberic-runtime/src/replicator/v1/operation_queue.rs#L144-L157
[secondary-progress]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/kuberic-runtime/src/replicator/v1/secondary_receiver.rs#L369-L385
[secondary-progress-test]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/kuberic-runtime/tests/support/read_fencing.rs#L76-L139
[not-ready]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/kuberic-runtime/src/replicator/v1/secondary.rs#L340-L374
[telemetry]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/kuberic-runtime/src/host/hosting.rs#L80-L101
[opened]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/kuberic-runtime/src/replicator/v1/service_adapter.rs#L237-L262
[open-test]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/kuberic-runtime/src/replicator/v1/role/tests/open.rs#L222-L248
[query-matrix]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/kuberic-runtime/tests/classic_v1_production.rs#L991-L1085
[query-snapshots]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/kuberic-runtime/tests/classic_v1_production.rs#L1131-L1168
[epoch]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/kuberic-runtime/src/replicator/v1/replica_manager.rs#L292-L302
[catchup-doc]: ../../../kuberic-runtime/src/replicator/traits.rs#L107
[epoch-doc]: ../../../kuberic-runtime/src/traits.rs#L53
[secondary-epoch]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/kuberic-runtime/src/replicator/v1/role.rs#L399-L435
[data-loss]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/kuberic-runtime/src/replicator/v1/role/tests/data_loss.rs#L233-L252
[sentinel]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/kuberic-runtime/src/replicator/mod.rs#L153-L163
[program]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/kuberic-runtime/src/host/public_lifecycle_program.rs#L14-L44
[stable-secondary]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/kuberic-runtime/src/host/v1_lifecycle.rs#L187-L195
[native-close-start]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Failover/ra/FailoverUnitProxy.CloseAsyncOperation.cpp#L94-L105
[native-close-end]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Failover/ra/FailoverUnitProxy.CloseAsyncOperation.cpp#L206-L275
[close-join]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/kuberic-runtime/src/replicator/v1/service_adapter/tests/facade.rs#L351-L387
[kv-close]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/examples/kvstore2/src/service.rs#L200-L218
[kv-close-test]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/examples/kvstore2/tests/read_fencing.rs#L478-L532
[remove]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/kuberic-runtime/src/replicator/v1/replica_manager.rs#L765-L789
[remove-test]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/kuberic-runtime/src/replicator/v1/primary/tests.rs#L1234-L1274
[pg-access]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/examples/postgres/src/public_v1.rs#L626-L731
[pg-access-test]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/examples/postgres/src/public_v1_p6_tests.rs#L3-L158
[pg-tick]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/examples/postgres/src/public_v1.rs#L412-L421
[pg-catchup]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/examples/postgres/src/public_v1.rs#L1476-L1504
[pg-catchup-test]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/examples/postgres/src/public_v1_p6_tests.rs#L253-L368
[fallback]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/kuberic-controller/src/evaluator/public_v1_topology.rs#L445-L470
[fallback-test]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/kuberic-runtime/tests/public_v1_recovery.rs#L4396-L4412
[pg-remove]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/examples/postgres/src/public_v1.rs#L1919-L1952
[pg-slots]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/examples/postgres/src/native.rs#L212-L258
[pg-remove-test]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/examples/postgres/src/public_v1_tests.rs#L607-L688
[pg-stop]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/examples/postgres/src/public_v1.rs#L567-L617
[pg-progress]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/examples/postgres/src/public_v1.rs#L1507-L1525
[pg-stop-test]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/examples/postgres/src/public_v1_p6_tests.rs#L163-L249
[pg-abort]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/examples/postgres/src/public_v1.rs#L500-L516
[pg-abort-test]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/examples/postgres/src/public_v1_tests.rs#L940-L1050
[policies]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/kuberic-runtime/src/replicator/v1/service_adapter/production.rs#L40-L66
[production-policy]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/kuberic-runtime/src/replicator/v1/service_adapter/production.rs#L121-L190
[buffer-limits]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/kuberic-runtime/src/replicator/v1/service_adapter/production_transport.rs#L35-L43
[settings]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/kuberic-runtime/src/replicator/mod.rs#L275-L283
[effective]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/kuberic-runtime/src/replicator/mod.rs#L390-L438
[effective-test]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/kuberic-runtime/tests/classic_v1_production.rs#L451-L502
[native-defaults]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/ServiceModel/reliability/replication/REConfigMacros.h#L136-L207
[native-settings]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Replication/ComReplicator.cpp#L762-L808
[partition]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/kuberic-runtime/src/replicator/mod.rs#L1036-L1064
[http]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/kuberic-runtime/src/replicator/v1/service_adapter/production_transport.rs#L177-L186
[server]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/kuberic-runtime/src/replicator/v1/service_adapter/production_transport.rs#L217-L227
[bearer]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/kuberic-runtime/src/replicator/v1/service_adapter/production_transport.rs#L728-L766
[control-auth]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/kuberic-runtime/src/host/service.rs#L1212-L1224
[dispatch]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/kuberic-runtime/src/replicator/v1/service_adapter/production_transport.rs#L365-L400
[authority]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/kuberic-runtime/src/host/native_transport.rs#L148-L226
[ingress-test]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/kuberic-runtime/src/replicator/v1/service_adapter/tests/production_ingress.rs#L226-L370
[protocol]: https://github.com/youyuanwu/kuberic/blob/aad23335c219a576d69133cee883bbd63e40b68a/kuberic-runtime/src/protocol/public_operations.rs#L13
[selected-policy]: https://github.com/youyuanwu/kuberic/blob/69b1cfe3c85d296463da0def3e80eaca0ddcd0a7/kuberic-runtime/src/host/selected_access.rs#L209-L300
[selected-activation]: https://github.com/youyuanwu/kuberic/blob/69b1cfe3c85d296463da0def3e80eaca0ddcd0a7/kuberic-runtime/src/host/selected_access.rs#L721-L766
[selected-tests]: https://github.com/youyuanwu/kuberic/blob/69b1cfe3c85d296463da0def3e80eaca0ddcd0a7/kuberic-runtime/tests/support/staged_access.rs#L144-L2159
[reconciliation-tests]: https://github.com/youyuanwu/kuberic/blob/69b1cfe3c85d296463da0def3e80eaca0ddcd0a7/kuberic-runtime/tests/support/staged_access.rs#L919-L1545
[producer-tests]: https://github.com/youyuanwu/kuberic/blob/69b1cfe3c85d296463da0def3e80eaca0ddcd0a7/kuberic-runtime/tests/support/staged_access.rs#L484-L599
[selected-close]: https://github.com/youyuanwu/kuberic/blob/aad23335c219a576d69133cee883bbd63e40b68a/kuberic-runtime/src/host/public_lifecycle_program.rs#L550-L638
[selected-recovery]: https://github.com/youyuanwu/kuberic/blob/aad23335c219a576d69133cee883bbd63e40b68a/kuberic-runtime/src/host/public_lifecycle_terminal.rs#L79-L115
[selected-close-tests]: https://github.com/youyuanwu/kuberic/blob/aad23335c219a576d69133cee883bbd63e40b68a/kuberic-runtime/tests/public_v1_recovery.rs#L3114-L3505
[availability]: https://github.com/youyuanwu/kuberic/blob/aad23335c219a576d69133cee883bbd63e40b68a/kuberic-runtime/src/protocol/public_operations.rs#L548-L562
[availability-digest]: https://github.com/youyuanwu/kuberic/blob/aad23335c219a576d69133cee883bbd63e40b68a/kuberic-runtime/src/protocol/public_operations.rs#L3118-L3133
[native-stage-selector]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Failover/ra/FailoverUnitProxy.ReadWriteStatusCalculator.cpp#L288-L381
[schema]: ../../../kuberic-runtime/src/host/state.rs#L19-L21
[native-send-errors]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Replication/RemoteSession.cpp#L753-L790
[native-ack-errors]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Replication/PrimaryReplicator.cpp#L633-L740
[rejection-owner]: ../../../kuberic-runtime/src/replicator/v1/primary/tests/rejection_order.rs#L18-L230
[rejection-public]: ../../../kuberic-runtime/tests/public_v1_recovery/rejection_order.rs#L57-L392
[local-fault-entry]: ../../../kuberic-runtime/src/replicator/v1/replica_manager.rs#L793-L1045
[pending-native-wait]: ../../../kuberic-runtime/tests/public_v1_recovery.rs#L590-L625
[public-context]: https://github.com/youyuanwu/kuberic/blob/45749d3744b43095be9bc9ec3fb4abf20633a537/scripts/fixtures/classic-v1-provider/tests/public_copy_context.rs#L377-L647
[context-default]: https://github.com/youyuanwu/kuberic/blob/45749d3744b43095be9bc9ec3fb4abf20633a537/scripts/fixtures/classic-v1-provider/src/lib.rs#L369-L411
