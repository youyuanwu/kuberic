# Classic V1: Current Semantics

This is the authoritative behavioral reference for the selected classic V1
runtime. Its source/test citations are pinned to Kuberic
`e2a47b6c631c974aa0135e81c3a7e3a439980ded`; the documentation rewrite changes no
runtime or test behavior. Native comparisons use Service Fabric
`3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d`.

**Evidence is bounded:** inspected source and existing Rust regression bodies
are not a fresh test run, native binary differential, power-loss proof,
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
([test:1013–1091][context-test]). The public external provider uses empty
context, so it does not qualify that distinct-nonempty public case
([fixture:196–207][empty-context]).

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
[actual tests:641–719][catchup-test]. Do not infer them from the broader stale
protected catch-up Rustdoc; comment reconciliation is
[pending](sf-v1-semantic-alignment-plan.md#p2--source-contract-wording).

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
([manager:292–302][epoch], [role:399–435][secondary-epoch]).
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

Two **verified current core differences**, not broad deferred P7 extensions,
remain:

1. **Staged access:** production uses completed-intent readiness and conservative
   pending/fence projection, with a pre-Revoke Swap exception
   ([host:551–576][access-ready], [692–756][access-projection]).
   Native preserves reads during particular demotion/same-primary stages
   ([native:58–144][native-access]). The private staged selector is not wholly
   selected by the production journal executor. This is a conservative
   availability difference, not demonstrated unsafe admission.
2. **Outer Close:** selected production preserves child failure, aborts,
   retains cleanup and returns **failed outer Close**
   ([program:531–599][outer-close], [diagnostics test:1064–1078][close-test]).
   Native outer Close can succeed after aborting a failed child while retaining
   diagnostics ([native:94–105][native-close-start],
   [206–275][native-close-end]). A private host's successful outer result is not
   selected-host closure. See [prioritized work](sf-v1-semantic-alignment-plan.md#p1--verified-core-alignment-differences).

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

| Boundary | Current behavior and evidence | Limit |
|---|---|---|
| SQL access | Samples live partition status before cache reuse; primary SQL requires read+write Granted; an otherwise eligible secondary needs receive **and replay** at its source boundary ([PG:626–731][pg-access], [tests:3–158][pg-access-test]) | Owned periodic reconciliation is not per-statement mediation or instantaneous revocation ([PG:412–421][pg-tick]). Isolated secondary test status is not host-granted stable-secondary access. |
| Catch-up | **Marked-successor WriteQuorum explicitly rejects before waiting**; ordinary unmarked WriteQuorum and All are supported ([PG:1476–1504][pg-catchup], [tests:253–368][pg-catchup-test]) | Unsupported is not implemented/ignored. Capability-false swap chooses All ([controller:445–470][fallback], [public witness:4244–4255][fallback-test]). |
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
| Data | Raw single buffer, one segment, payload cap 52,428,800 bytes; private lossless segmented codec is not a public selectable policy |
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

Public protocol 12 and agent schema 13 are breaking formats, not native wire
versions ([protocol:13][protocol], [schema:19–21][schema]).
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
[secondary-epoch]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/kuberic-runtime/src/replicator/v1/role.rs#L399-L435
[data-loss]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/kuberic-runtime/src/replicator/v1/role/tests/data_loss.rs#L233-L252
[sentinel]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/kuberic-runtime/src/replicator/mod.rs#L153-L163
[program]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/kuberic-runtime/src/host/public_lifecycle_program.rs#L14-L44
[stable-secondary]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/kuberic-runtime/src/host/v1_lifecycle.rs#L187-L195
[access-ready]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/kuberic-runtime/src/host/hosting.rs#L551-L576
[access-projection]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/kuberic-runtime/src/host/hosting.rs#L692-L756
[native-access]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Failover/ra/FailoverUnitProxy.ReadWriteStatusCalculator.cpp#L58-L144
[outer-close]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/kuberic-runtime/src/host/public_lifecycle_program.rs#L531-L599
[close-test]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/kuberic-runtime/src/host/public_lifecycle_tests.rs#L1064-L1078
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
[fallback-test]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/kuberic-runtime/tests/public_v1_recovery.rs#L4244-L4255
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
[protocol]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/kuberic-runtime/src/protocol/public_operations.rs#L13
[schema]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/kuberic-runtime/src/host/state.rs#L19-L21
