# Kuberic Test Strategy

Kuberic validation is split into exact nextest tiers, workspace checks,
provider-backed smoke tests, and explicitly owned Kubernetes scenarios.

The [faithful SF V1 Rust async/tonic port plan](sf-v1-semantic-alignment-plan.md)
adds native-source/test-vector mapping and a future behavior/capability matrix:
persisted/volatile services, service-owned primary commit, receiver lifetimes,
progress/quorum/status, terminal outcomes and real tonic retry/reconnect/
backpressure scenarios. Existing KSF inventory and current-contract regressions do
not establish complete native parity. P1's documented acceptance scenarios are
separate from future executable port-component/hosted cases; add selectors
for those only with actual implementations. The unselected OperationQueue,
bounded sender/session/ACK, async/tonic, primary queue/quorum, secondary receiver/ACK
copy-receiver, enumeration, CopySender build-gate, copy-establishment, BuildIdle,
secondary and primary lifecycle, configuration, role/recovery, queue-policy and
message-ingress slices have
actual component tests below. Preserve
safety outcomes while revising assertions that encode intentionally different
current contracts.

## Workspace Checks

```bash
cargo fmt --all -- --check
cargo check --workspace --all-targets --all-features
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --doc --workspace --all-features
scripts/check_runtime_public_api.sh
```

The API guard compares documented and source-public inventories and runs an
external privacy fixture. It protects the public interface boundary without
exporting host capabilities.

## Exact Nextest Inventory

```bash
just nextest-validate
```

The validator reports the current test and partition counts, proves every test
has exactly one ordinary, source, PostgreSQL, or KinD tier, and verifies that every
configured partition union is complete. Discovery builds the tier binaries but
does not execute PostgreSQL or KinD bodies. Doctests are additional source-job
checks, not nextest archive entries.

## Ordinary Tier

```bash
just nextest-test
```

The ordinary tier is cluster-free. It includes runtime/controller behavior,
real SQLite and KVStore2 providers, immutable old-state rejection, Public V1
wire/history/session/evidence coverage, task reclamation, writer fencing, and
the structural KSF catalog.

Timeout-sensitive hosted recovery tests should use the repository nextest
profile rather than an ad-hoc highly parallel `cargo test` invocation.

### SF V1 P1 Foundation

```bash
scripts/check_runtime_public_api.sh
```

P1 adds source-backed acceptance scenarios in the
[port ledger](sf-v1-port-ledger.md), covering source families, contract mappings,
deterministic scenarios and capability/settings cases. They are
documented expectations for future tests of actual ported components, not new
ordinary tests or executable formula checks. The strengthened existing external
API fixture above checks real protected-interface compatibility. The existing
KSF entries remain structural/current-contract evidence; no new KSF ID or
native-parity claim is introduced.

The separate current-engine counterexample selection is:

```bash
cargo nextest run -p kuberic-runtime --all-features --profile ordinary \
  --test-threads 1 --test public_v1_recovery \
  -E 'test(=read_fencing::sf_d02_primary_scalar_waits_for_quorum_and_local_commit_across_cancel_and_epoch) or test(=read_fencing::sf_d02_secondary_scalar_is_contiguous_service_acknowledged_not_committed) or test(=hosted_public_v1_contained_close_restarts_without_replaying_interrupted_children)'
```

These existing real-engine tests preserve current held-ACK/held-local-
commit and contained-Close assertions. They do not execute expected native
algorithms. Further current-source counterexamples in the ledger remain
source-only. Memory-backed current-engine witnesses prove ordering, not fsync.
The strengthened existing external API fixture pins Bytes, boxed `Stream<Item = Result<Bytes>>
+ Send`, i64 and all protected methods; it remains one of the existing
build-environment guards, now selected by `just nextest-source`,
not a new ordinary or archive-only Cargo invocation.

### SF V1 OperationQueue Slice

```bash
cargo nextest run -p kuberic-runtime --lib --all-features --profile ordinary \
  -E 'test(replicator::v1::operation_queue::tests::)'
```

This selects descriptive tests of the actual internal
[`OperationQueue`](../../../kuberic-runtime/src/replicator/v1/operation_queue.rs#L212)
and its
[`component tests`](../../../kuberic-runtime/src/replicator/v1/operation_queue/tests.rs#L73).
They cover separate tail/commit/completion/history boundaries, holes/duplicates,
count-span and byte pressure (including history removal on failed admission),
retention, callback timing/replay, rewinds, trim, reset and transfer. Q1–Q4/L2
remain provenance identifiers, not a duplicate executable catalog.
The original queue tests are joined by async/tonic-substrate adapter
regressions proving framed-operation logical-byte accounting without changing
the original raw-byte behavior.

The tests belong to ordinary and the existing all-feature archive automatically;
no new partition exemption or ignored tests are introduced. Source and archive
inventories must agree. This is source-guided Rust component evidence, **not**
native execution, differential parity, role/owner integration, full P2 or P5.
The queue remains unselected and the protected compiler fixtures are unchanged.
See the [ledger](sf-v1-port-ledger.md#operationqueue-slice-p2) and
[selected translation provenance](sf-v1-port-provenance.md#operationqueue-implementation-slice-p2-not-all-of-p2).

### SF V1 Sender Session ACK Slice

```bash
cargo nextest run -p kuberic-runtime --lib --all-features --profile ordinary \
  -E 'test(replicator::v1::sender::tests::) or test(replicator::v1::ack::tests::) or test(replicator::v1::session::tests::)' -j 2
# All preparatory v1 components, including the existing queue:
cargo nextest run -p kuberic-runtime --lib --all-features --profile ordinary \
  -E 'test(replicator::v1::)' -j 2
```

The first selects actual sender, ACK scheduler and bounded-session component
tests; the all-v1 selector also includes the queue and later async/tonic substrate
below. They invoke real private compiled components with injected/manual time,
no guessed-deadline sleeps and no formula/catalog oracle.
Sender tests also invoke its production latency/decay helpers. They exercise
receive/apply separation, ordered replay/dedup, count windows, exact retries,
RequestAck, hints/backpressure, batching/force/final pending flush, trace,
coalescing/copy terminal state, reentrant dispatch and close/root lifetimes.
The initiating callback and processor ownership survive coalesced ACK arrivals.

Deadlines and timer-callback entry are component seams, not a live timer adapter.
Session copy/primary effects are explicitly delivered handoffs, **not execution
of missing copy/build/quorum owners**. Tests demonstrate component order,
ownership and progress; they do not prove demux, carrier/error mapping, provider
durability or native/hosted parity. Native tests/sources were read, not executed.
The production factory, public aliases and protected interfaces are unchanged.
These tests are ordinary, automatically discoverable by the existing archive/
partition rules; no ignored test, catalog ID, tier exemption or dependency added.
See [scope/remaining owners](sf-v1-port-ledger.md#sender-session-ack-slice-p2-bounded-f06)
and [source/test provenance](sf-v1-port-provenance.md#sender-session-ack-implementation-slice-p2-bounded-f06).

### SF V1 Async Tonic Substrate Slice

```bash
cargo nextest run -p kuberic-runtime --lib --all-features --profile ordinary \
  -E 'test(replicator::v1::buffers::) or test(replicator::v1::transport::) or test(replicator::v1::async_runtime::)' -j 2
```

This selects actual private async runtime, carrier and buffer adapter tests.
Queue-accounting regressions are selected above. A cross-component test connects
the actual session runtime, send-intent adapter, tonic exchange, reverse ACK
runtime and ordered session
ACK processor; required copy-owner handoffs are recorded, not fabricated.
Explicit raw-single/framed-segments cases preserve arbitrary bytes,
zero/empty/multiple buffers, per-item stream errors
and ordinary EOF. Malformed frames fail rather than becoming empty success.
Carrier cases keep routing/incarnation/stream ownership, independent receive/
apply ACKs, per-message expiry, count/byte pressure and failure outcomes
separate from service/quorum completion. Paused Tokio time drives actual
session retry and ACK batching/force/close callbacks, not test-only formulas
or guessed wall-clock sleeps.

The private proto/service is built but not registered with the selected host.
Public types, factory selection and current RPCs remain unchanged. The
external negative compiler fixture explicitly rejects access to `replicator::v1`.
No native test/binary, production activation, copy/build/quorum owner, full P2
or hosted/native/differential parity is claimed. See the
[bounded ledger](sf-v1-port-ledger.md#async-tonic-substrate-slice-p2-bounded-f05f08f11)
and [provenance](sf-v1-port-provenance.md#async-tonic-substrate-slice-p2-bounded-f05f08f11).

### SF V1 Primary Queue and Quorum Slice

```bash
cargo nextest run -p kuberic-runtime --lib --all-features --profile ordinary \
  -E 'test(replicator::v1::replication_queue_manager::) or test(replicator::v1::replica_manager::)' -j 2
```

These tests invoke actual private managers, queue entries, sender sessions and
ordered ACK processors. Successful assignment is observable before quorum
completion; full/malformed admission does not consume an LSN. Held ACKs,
configuration changes and outside-owner dispatch exercise current/previous
quorum, quorum-one asymmetry, receive-based reclamation versus apply-based
catch-up, idle retention, configuration capture/caps and moving MustCatchup
requirements. Dropped waiters do not roll back admitted operations; terminal
completion and close outcomes remain distinct.

The primary queue owner, replica manager and integrated runtime library
selections passed. Workspace checks, warnings-denied Clippy,
no-default-feature runtime, doctests,
external API/privacy and exact tier/partition inventory passed. This is not a
claim that all ordinary tests were rerun.

Review regressions preserve cached CC quorum/all observations until the ordered
primary-progress handoff, concrete session-root identity, error-atomic
configuration/idle removal, and shutdown errors after terminal cleanup.
Out-of-range session ACKs cannot become valid by admitting more operations
while configured quorum membership is insufficient.

Queue-only transfer tests preserve retained original epochs and the issued-tail
assignment gap. They do not execute full promotion, demotion trimming, provider
epoch callbacks or receiver drain. Concrete established-session inputs are not
the full native descriptor-driven configuration/build lifecycle. No selected
factory, protected interface, service persistence hook, dependency, ignored
test, archive exemption or duplicate executable scenario catalog is added.
Slow-fault policy/notification, full role/copy/secondary owners, native or
differential execution, hosted parity and activation remain unestablished.
See the [bounded ledger and owner map](sf-v1-port-ledger.md#primary-queue-and-quorum-slice-p3-bounded-f02f06f09)
and [selected provenance](sf-v1-port-provenance.md#primary-queue-and-quorum-slice-p3-bounded-f02f06f09).

### SF V1 Secondary Receiver and ACK Slice

```bash
cargo nextest run -p kuberic-runtime --lib --all-features --profile ordinary \
  -E 'test(replicator::v1::received_operation::) or test(replicator::v1::operation_stream::) or test(replicator::v1::secondary_receiver::)' -j 2
```

The received-operation, operation-stream, receiver/integration and integrated
runtime library selections passed. Workspace all-target/all-feature check,
warnings-denied Clippy, no-default
runtime, doctests, external API/privacy and exact tier/partition inventory passed.
This is not a claim that all ordinary tests were rerun.

Actual queues and callbacks exercise required versus optimistic completion,
out-of-order data/service ACKs, duplicate/root identity, logical byte/span
pressure, completed hints and capability. Actual shared streams exercise
wait/wake/cancellation, reacquisition, EOF/EOS and explicit epoch handoffs.
Provider invocation is not a successful stub: full barriers pause until the
captured ACK boundary and explicit matching completion; stream markers block
all getters. Tests preserve original versus sending/copy epochs and callback
replay through successive barriers. Abandoned work does not auto-complete.

Real AckSender tests use copy-plus-replication pending thresholds and current
receiver snapshots. A real primary queue/manager/send/receiver/ACK/session
trace proves an ACK cannot bypass the ordered primary notification; its
unimplemented copy-owner handoff stays explicitly unresolved. It does not
execute build/role/provider integration or a selected hosted engine.

Receiver failure containment and sticky stream abort are explicit Rust adapter
boundaries; native fault reports, two-receiver EOS/drains, provider callbacks,
copy/context/build and full secondary role transitions remain future owners.
No protected interface, selected factory/wire, dependency, archive exemption,
ignored test or duplicate executable scenario catalog is added. Native binary/
test/differential, provider-fsync and hosted parity are not established.
See the [bounded ledger](sf-v1-port-ledger.md#secondary-receiver-and-ack-slice-p3-bounded-f04f05f06f09)
and [source provenance](sf-v1-port-provenance.md#secondary-receiver-and-ack-slice-p3-bounded-f04f05f06f09).

### SF V1 Copy Receivers Slice

```bash
cargo nextest run -p kuberic-runtime --lib --all-features --profile ordinary \
  -E 'test(replicator::v1::secondary_copy_receiver::) or test(replicator::v1::copy_context_receiver::) or test(replicator::v1::copy_receiver_integration::) or test(numbered_copy_eos_)' -j 2
```

The secondary-copy, copy-context, numbered-EOS constructor, pair/session
integration and integrated runtime library selections passed.
Workspace all-target/all-feature check, warnings-denied Clippy, no-default
runtime, doctests, external API/privacy and the exact
tier/partition inventory passed. Full ordinary, PostgreSQL and KinD
test bodies were not rerun.

Actual secondary-copy/context receivers reuse OperationQueue, ReceivedOperation,
SharedDispatchQueue and AckSender. Cases exercise count-bound gaps/duplicates/
retries, required versus optimistic completion, canonical numbered network EOS,
legacy last markers, logical-empty framed payloads, shared consumption and
terminal reacquisition. Callback selection/entry/completion, late exact identity,
root/destructor/waker reentry, local cleanup differences and rejected inputs
are exercised on real owners, not a copied ledger/formula model.

Context cases exercise target-to-source data receipt without service ACK,
strict pending threshold, current issuance snapshots, final error ACK and
explicit-close versus protocol-EOF outcomes. Pair/session witnesses exercise
the actual combined copy+replication pending threshold and terminal ACK
sentinels, stopping before the missing copy-build owner.

Provider invocation, context acquisition/copy enumeration/sending/build,
StartCopy establishment, cross-receiver fault/report/EOS/drains and full role
integration remain future owners. No protected type, selected production path,
dependency, ignore/exemption or native/differential/hosted parity claim changes.
See the [bounded ledger](sf-v1-port-ledger.md#copy-receivers-slice-p3-bounded-f04f05f07)
and [source provenance](sf-v1-port-provenance.md#copy-receivers-slice-p3-bounded-f04f05f07).

### SF V1 Copy Enumeration Slice

```bash
cargo nextest run -p kuberic-runtime --lib --all-features --profile ordinary \
  -E 'test(replicator::v1::copy_async_operation::)' -j 2
```

The copy-enumeration and integrated runtime library selections passed.
Workspace all-target/all-feature check, warnings-denied Clippy,
runtime no-default-feature check, external API/privacy, workspace doctests and
exact tier/partition discovery passed. Full ordinary, PostgreSQL and
KinD test bodies, native execution/differential, hosted parity and provider-fsync
were not run or claimed.

Actual CopyAsyncOperation tests use the real OperationQueue, reliable sender,
buffer adapter and injected clock. They exercise serial ready/pending provider
polls, empty data versus numbered EOF, count growth, one parked item, received
versus applied ACKs, duplicate/regressive ACKs, parked EOF notification, retry
and RequestAck, native cancellation timing and late provider-error precedence.
Native twelve-item drained-success and fifteen-item/eight-slot/full-retry
cancellation cases are adapted without the native harness. Real-executor
cases prove ACK wake/resume and in-flight cancellation; dropping a drive future
preserves its pending provider poll, not a successful native cancellation.

Rooted selected work retains the provider through End/owner release. Provider
polls, EOF/send callbacks, driver wakers and rejected/retired payload destructors
are exercised for outside-guard reentry. Framed-empty data, malformed settings/
ACK/frame and sequence exhaustion have actual regressions. Actual secondary
copy receivers exercise both service-ACK modes; actual primary-side context
receipt hides the protocol terminal and ACKs its LSN.

Final-copy readiness stays separate from an explicit outer-owner success request.
Receiver-boundary tests do not implement CopySender's sentinel conversion,
replication build gate, EOF-issued target or retargeting. Provider invocation/
context acquisition, StartCopy establishment, full fault/drain/role integration
and slow-provider API monitoring/health reporting remain pending, as does
production activation. No protected API, selected factory/
wire, dependency or tier/exemption policy changes.
See the [bounded ledger](sf-v1-port-ledger.md#copy-enumeration-slice-p3-bounded-f05f07f11)
and [source provenance](sf-v1-port-provenance.md#copy-enumeration-slice-p3-bounded-f05f07f11).

### SF V1 CopySender Build Gates Slice

```bash
cargo nextest run -p kuberic-runtime --lib --all-features --profile ordinary \
  -E 'test(replicator::v1::copy_sender::)' -j 2
```

The CopySender and integrated runtime library selections passed.
Workspace all-target/all-feature check, warnings-denied Clippy, no-default
runtime, external API/privacy, workspace
doctests and exact tier/partition discovery passed. Discovery does
not execute PostgreSQL/KinD bodies. Full ordinary execution, native/differential,
hosted parity and provider-fsync were not run or claimed.

Tests execute CopySender wrapping the real CopyAsyncOperation, not redefined
completion formulas. Both ACK arrival orders, actual session handoff ordering,
empty versus terminal data, sentinel/numeric terminal ACKs, applied-only
progress, parked EOF capture, malformed ACKs and real retry/queue-full behavior
are covered. An actual primary queue proves EOF captures the latest issued,
not committed/start-time LSN; a real session/queue witness proves upward pending
retarget releases retention without declaring success. Barrier-synchronized
threads test retarget/gate serialization; a reentrant final-payload destructor
deterministically races selected success against disabling completion and
requires the native End failure/fault. Native context mode, pre-Begin failures,
distinct cancellation/FinishOperation timing, first completion, capture/provider
errors, idempotent End, delegated progress/latency, rooted selected work and a
real executor wakeup are exercised. Actual secondary copy receivers cover both
service-ACK modes; actual primary context receipt supplies its numeric terminal
ACK. No test-only sentinel normalization or build-success flag substitutes for
the new owner.

Provider/context acquisition, reliable StartCopy establishment, full BuildIdle/
session/role/drain orchestration, periodic/slow-peer policy and monitoring remain
pending. Protected APIs and selected factory/service/wire/host/controller paths
are unchanged; the component is private and unselected.
See the [ledger](sf-v1-port-ledger.md#copysender-build-gates-slice-p3-bounded-f06f07f11)
and [provenance](sf-v1-port-provenance.md#copysender-build-gates-slice-p3-bounded-f06f07f11).

### SF V1 Copy Establishment Slice

```bash
cargo nextest run -p kuberic-runtime --lib --all-features --profile ordinary \
  -E 'test(replicator::v1::copy_establishment::)' -j 2
```

The copy-establishment and integrated runtime library selections passed.
Workspace check/Clippy, no-default runtime, external API/privacy, workspace
doctests and exact tier/partition discovery passed.

Tests exercise the actual reliable StartCopy sender, current-epoch/infinite
expiration mapping through a real tonic connection, sender-before-first-send
publication, reentrant immediate context response, retry/queue-full behavior and
selected sends surviving close. Target context travels through the real context
CopySender and primary CopyContextReceiver into the protected source provider;
the source provider's own GetCopyContext is never substituted. First context
establishes before EOF; nonpersisted ACK establishes with empty context.
The provider receives the frozen start-minus-one boundary while actual copy EOF
captures the distinct latest-issued replication target. Actual ordered session
ACK handoffs then satisfy the existing CopySender's build gates.

Wrong full endpoint/incarnation/partition/address, wrong replica, malformed
progress, stale/unexpected-new epoch, duplicate StartCopy, empty versus terminal
and framed context are covered. Provider acquisition failures retain
GetCopyContext/GetCopyState descriptions rather than fabricated EndGetNext
faults. Interrupted driver futures retain one in-flight acquisition;
cancellation closes incoming context to unblock GetCopyState but does not
declare a still-pending provider callback drained. Reentrant provider/ACK calls,
weak handles, callback roots and late-error/first-completion cases are exercised.

All components stay private/unselected; the caller still owns idle-role
eligibility, routes, the frozen committed boundary, ACK handoffs, acquisition/
enumeration drivers and remaining timer delivery. This is not full BuildIdle,
primary/secondary/role/drain orchestration, provider monitoring or activation.
Full workspace ordinary execution, PostgreSQL/KinD bodies, native execution,
differential/hosted parity and provider-fsync were not run or claimed.
See the [ledger](sf-v1-port-ledger.md#copy-establishment-slice-p3-bounded-f04f05f07f08f11)
and [provenance](sf-v1-port-provenance.md#copy-establishment-slice-p3-bounded-f04f05f07f08f11).

### SF V1 BuildIdle Lifecycle Slice

```bash
cargo nextest run -p kuberic-runtime --lib --all-features --profile ordinary \
  -E 'test(replicator::v1::build_idle::) or test(replicator::v1::replication_queue_manager::tests::build_snapshot_)' -j 2
```

The BuildIdle, primary-queue snapshot and integrated runtime library selections
pass. Workspace check/Clippy, no-default runtime, API/privacy, workspace doctests
and exact tier/partition discovery pass.

Actual manager admission freezes the classic committed/previous-quorum-floor
boundary, retains/replays the actual pending entries and creates/opens a fresh
session. The provider's frozen boundary and actual EOF-issued replication
completion target remain distinct. Tests cover target context, ordered ACK
handoffs and both build gates, success retaining idle replication, dynamic
retention release, duplicate/invalid/closed/exhausted admission, pre-coalescing
ACK validation, source acquisition versus enumeration faults, and subsequent
build rejection after a private recorded primary fault.

Reentrant replay/StartCopy/provider/context-ACK/removal/close callbacks run
outside guards. Selected StartCopy may outlive removal but cannot restart
retries or make the removed build succeed. BuildIdle context-only cancellation
differs from RemoveReplica/Close session cancellation, and interrupted/pending
provider acquisition stays owned until real completion. Old handles cannot
cancel same-ID replacement builds; abandoned owners are explicit errors and
do not prevent other close effects. Real pinned driver futures and paused Tokio
time exercise retry continuity after establishment and newly installed
context/replication deadlines, not just repeated drop-and-repoll tests.

All components stay private/unselected. Full primary/secondary/session/role
composition, cross-receiver/provider drains, slow-peer/promotion policy,
provider monitoring and host health reporting remain separate owners.
The build driver does not replace ongoing primary session ownership after
completion. Full workspace ordinary execution, PostgreSQL/KinD bodies,
native execution, differential/hosted parity and provider-fsync are not run
or claimed. Earlier slice exclusions are historical boundaries.
See the [ledger](sf-v1-port-ledger.md#buildidle-lifecycle-slice-p3-bounded-f02f05f06f07f11)
and [provenance](sf-v1-port-provenance.md#buildidle-lifecycle-slice-p3-bounded-f02f05f06f07f11).

### SF V1 Secondary Lifecycle Slice

```bash
cargo nextest run -p kuberic-runtime --lib --all-features --profile ordinary \
  -E 'test(replicator::v1::secondary::) or test(replicator::v1::operation_stream::tests::lifecycle_)' -j 2
```

Actual components exercise shared StartCopy initialization, required/optimistic
service ACKs, context acquisition/sender completion and retries, provider epoch
calls from full ACK barriers versus stream markers, matching completion and
cross-receiver fault cleanup. Legacy Close aborts dispatch without waiting for
held data ACKs or pending GetCopyContext; EOS Close drains initialized copy then
replication streams, including required EOS ACKs, before context/epoch waits.
Uninitialized streams use native abort bypass. Incomplete copy clears unpumped
replication before EOS. A failed copy drain returns its error without waiting
for replication/provider drainage; subsequent Abort still waits actual pending
provider calls. Actual provider faults retain their distinct aftermath and
remain observable separately from Close/Abort results.

Pinned driver futures and paused Tokio time exercise new ACK deadline
resampling, live progress snapshots and context retry continuity. Reentrant ACK
and provider callbacks execute outside guards. Interrupted driver polls retain
actual provider futures, and cleanup callback tokens retain the provider without
root/receiver cycles. Dropping the explicit owner is abandonment, not drain.
Neither Close nor Abort claims arbitrary in-flight provider GetNext drainage.

The owner is private/unselected; typed data ingress assumes caller-owned
route/epoch admission. Full primary/session/role orchestration, external epoch
reconfiguration, promotion, slow-peer policy, provider monitoring/host health
and P4/P5 adapters/activation remain pending. Full ordinary execution,
PostgreSQL/KinD bodies, native execution, differential/hosted parity and
provider-fsync are not run or claimed. Earlier exclusions remain
historical. See the [ledger](sf-v1-port-ledger.md#secondary-lifecycle-slice-p3-bounded-f04f05f06f09f11)
and [provenance](sf-v1-port-provenance.md#secondary-lifecycle-slice-p3-bounded-f04f05f06f09f11).

### SF V1 Primary Lifecycle Slice

```bash
cargo nextest run -p kuberic-runtime --lib --all-features --profile ordinary \
  -E 'test(replicator::v1::primary::)' -j 2
```

The decisive case finishes an actual BuildIdle, supplies that same session
through the explicit caller-established configuration seam, then admits more
replication, observes actual retransmission and completes the actual quorum
waiter through bound ACK ingress. Separate cases keep a finished build idle
without inventing a quorum vote. Other cases exercise receive-only versus apply
ACKs/RequestAck, CC/PC session deduplication, queue-full retry, stale route and
incarnation rejection, atomic configuration failure, reentrant/coalesced ACK
handoffs and timer deadlines installed into a continuously pinned driver.
CC/PC lookup takes precedence over retained idle membership, preserving
replication-only ACK ingress before and after removal from the idle set.

Ordinary Close optionally waits actual pending replicate quorum completion;
timeout continues native Close, never fabricates an ACK or fails Close solely
for the prewait timeout. Infinite waits avoid monotonic-clock overflow. Abort
skips/preempts the prewait and remains independently observable. Selected
send/ACK/manager effects drain before primary terminal completion; client
oneshot consumption is not required. Actual pending provider build acquisition
survives interrupted driver polls, removal and primary Close; explicit build
drainage waits its real callback and preserves actual provider errors.
Weak ingress rejects owner abandonment; removed builds/providers are released
after actual completion without aliasing a replacement with the same replica ID.

Workspace check/Clippy, no-default/API/privacy, workspace doctests and exact
inventory gates pass. All components remain private/unselected. At that slice
boundary configuration was a caller-established seam, not native
UpdateConfiguration/session creation or promotion. Role/external-epoch
orchestration, queue transfer, slow-peer/MustCatchup policy, host health and P4/P5
activation remain pending.
No full ordinary execution, PostgreSQL/KinD bodies, native binary execution,
differential/hosted parity or provider-fsync is run/claimed. Earlier exclusions
are historical. See the
[ledger](sf-v1-port-ledger.md#primary-lifecycle-slice-p3-bounded-f02f06f07f08f09f11)
and [provenance](sf-v1-port-provenance.md#primary-lifecycle-slice-p3-bounded-f02f06f07f08f09f11).

### SF V1 Primary Configuration Slice

```bash
cargo nextest run -p kuberic-runtime --lib --all-features --profile ordinary \
  -E 'test(replicator::v1::primary::tests::configuration::)' -j 2
```

Descriptor-driven catch-up and current-only updates create/open actual sessions
from known progress, reuse current/previous/idle roots, replay retained operations,
pause new previous-only sessions with missing history and retire unreferenced
senders. Unknown-progress current members need existing current or idle membership,
not merely a previous-only session. Native configuration matching is by replica
ID: changed descriptor addresses/incarnations/progress do not rebind an existing
session. Full bound ACK identity checks still reject stale/replacement ingress.

Actual tests cover seeded replay and original epochs, shared CC/PC roots,
duplicate updates and contraction, missing history and atomic invalid-input
rejection, independent current/previous quorum constraints and pending catchup
reevaluation. Idle-to-active session promotion preserves the actual build/sender,
marks promotion and resets actual latency averages once while retaining live
latency watches. A pending provider/build is not fabricated successful or silently
drained by promotion; explicit cleanup continues the actual owner.
Reentrant replay can ACK, reconfigure/remove or Abort; ordinary Close waits a
selected configuration send. Queue-full and pinned-driver retry cases exercise
the existing real reliable sender.

Workspace check/Clippy, no-default/API/privacy, workspace doctests and exact
inventory gates pass. The established-session seam remains for isolated callers,
but the new configuration entry points construct their own sessions.
All owners remain private/unselected. At that slice boundary, secondary-to-primary role promotion,
external epochs/drained queue transfer, periodic/slow-peer policy, host health and
P4/P5 activation remain pending. No full ordinary execution, PostgreSQL/KinD
bodies, native execution, differential/hosted parity or provider-fsync claim.
See the [ledger](sf-v1-port-ledger.md#primary-configuration-slice-p3-bounded-f02f06f08f09)
and [provenance](sf-v1-port-provenance.md#primary-configuration-slice-p3-bounded-f02f06f08f09).

### SF V1 Role Transition Slice

```bash
cargo nextest run -p kuberic-runtime --lib --all-features --profile ordinary \
  -E 'test(replicator::v1::role::tests::) | test(replicator::v1::secondary::tests::) | test(replicator::v1::replication_queue_manager::tests::)' -j 2
```

The affected-owner selector exercises the actual role owner, secondary
drain/epoch paths and demotion normalization alongside their existing component
regressions. Use the exact inventory validator above for current test and
partition counts rather than retaining a static slice total.

Actual role edges cover initial progress acquisition/caching, primary epoch
completion before role publication, idle-to-active dispatch drainage, promotion
and demotion with real queues, and terminal role None. Required ACK promotion
waits actual service callbacks; optimistic promotion waits dispatch consumption,
not an invented application ACK. Legacy incomplete-copy rejection differs from
EOS promotion's fully-received-copy/data/EOS ACK drainage. Provider barriers
already running when promotion begins remain retained.

Queue handoff preserves operation Arc identity and original epochs, does not
replay already-applied demoted history and installs the correct service ACK mode
for new secondary data. Demotion enforces actual count and logical-byte limits,
rejects uncommitted suffixes and cancels pending writes without fabricating ACKs.
New primaries still require actual configuration before accepting writes.

External primary epochs use issued tail, update session send epochs and call
the provider even for duplicates. Pending provider calls retain real retry
timers across interrupted polls. Secondary direct epochs instead discard actual
out-of-order work, raise the minimum and drain dispatch/ACKs without invoking
the provider themselves. Stale epochs, copy-in-progress transitions, actual
provider/build faults and Abort remain explicit outcomes. Role completion does
not claim pending build/provider enumeration drainage; retained old-primary
builds can be explicitly driven after handoff.

Workspace check/Clippy, no-default/API/privacy, doctests and exact inventory
gates pass. Protected APIs, selected wiring, dependencies and tier policy are
unchanged. No full ordinary execution, PostgreSQL/KinD bodies, native execution,
differential/hosted parity or provider-fsync is claimed. At that slice boundary
data-loss/recovery orchestration, periodic/slow-peer policy, host demux/health and P4/P5 activation
remain pending. See the
[ledger](sf-v1-port-ledger.md#role-transition-slice-p3-bounded-f01f02f04f09f11)
and [provenance](sf-v1-port-provenance.md#role-transition-slice-p3-bounded-f01f02f04f09f11).

### SF V1 Data Loss Recovery Slice

```bash
cargo nextest run -p kuberic-runtime --lib --all-features --profile ordinary \
  -E 'test(replicator::v1::role::tests::) | test(replicator::v1::primary::tests::) | test(replicator::v1::replica_manager::tests::) | test(replicator::v1::replication_queue_manager::tests::)' -j 2
```

This selector exercises the actual role, primary, manager and queue owners,
including their existing regressions. Use the live nextest inventory and exact
partition validator rather than a static slice total.

Data-loss checks are primary-only and serialize with role/epoch operations.
Previously issued primary handles and new ACK/context ingress are gated while
checking. Actual provider callbacks can reenter without owner/manager locks.
Unchanged recovery preserves pending writes, operation identity, original
epochs, queue/cached progress and real retry timers without a provider progress
query. Changed recovery queries actual progress, updates the cache before reset
and forwards reset into the existing queue. Higher, lower, equal and empty
progress drive actual subsequent LSN assignment and catch-up/quorum boundaries.

Pending writes, committed-but-uncompleted history, selected activities and
invalid progress reject reset without cancellation or invented ACKs. Callback/
query/reset failures retain the changed flag and restore primary admission;
original provider errors win over simultaneous restoration errors.
Interrupted polling retains each callback/query. Abort retains the recovery
chain, but not successful reset or recovery, and preserves provider failure.
Reported source build faults reject admission. Reentrant primary Abort and
concurrent primary Close cannot produce successful recovery.

Protected APIs, selected wiring, dependencies and tier policy are unchanged.
At that slice boundary broader host/Open recovery and rebuild policy, periodic/slow-peer/MustCatchup
policy, full host demux/health and P4/P5 activation remain pending.
No full ordinary execution, PostgreSQL/KinD bodies, native binary/differential/
hosted parity or provider-fsync is claimed. See the
[ledger](sf-v1-port-ledger.md#data-loss-recovery-slice-p3-bounded-f01f02f03f09f11)
and [provenance](sf-v1-port-provenance.md#data-loss-recovery-slice-p3-bounded-f01f02f03f09f11).

### SF V1 Queue Pressure and Slow-Peer Policy Slice

```bash
cargo nextest run -p kuberic-runtime --lib --all-features --profile ordinary \
  -E 'test(replicator::v1::primary::tests::) | test(replicator::v1::replica_manager::tests::) | test(replicator::v1::session::tests::) | test(replicator::v1::build_idle::tests::) | test(replicator::v1::copy_establishment::tests::) | test(replicator::v1::operation_queue::tests::) | test(replicator::v1::transport::tests::) | test(replicator::v1::role::tests::)' -j 2
```

The selector runs actual private owners and their existing regressions. New
policy cases drive paused Tokio time and actual queue admission/ACKs: freshness
strictly beyond the lag interval retargets to commitment, pressure retargets to
issued tail before disable/fault and rechecks freed capacity. Rejected full
admission still drives pressure without consuming an LSN. Utilization covers
pending span, holes, logical bytes and completed-history exclusion; transfer
refreshes actual operation age.

Actual sender latencies exercise fastest-quorum population deviation and its
strict boundary, matching least receive progress, age and spare-member guards.
Slow fault closes the sender before receive-based reclamation without changing
quorum/all-applied or the live MustCatchup minimum. A marked faulted replica can
keep catch-up pending; late ACKs cannot invent progress. Idle completion-disable
uses the actual CopySender; an already completed build cannot be faulted.
Faulted idle promotion is rejected.

Idle fault notification requires both terminal copy ACK sentinels. Retry checks
cover backpressure, accepted delivery, callbacks outside guards, removal and
Close/Abort. An actual tonic carrier delivers repeated typed InduceFault sends
with the route's real target identity and no synthetic sender ACK. Invalid
private timer settings and missing real copy ownership fail explicitly; policy
failure does not hide an already assigned write.

Use live nextest inventory/partition validation rather than static test totals.
Protected interfaces, selected wiring, dependencies and tier policy are
unchanged. Broader Open/restart/host recovery and rebuild, full host role/epoch/
InduceFault demux, remote fault handling, monitoring/health, public settings
and P4/P5 activation remain pending. No full ordinary execution, PostgreSQL/KinD,
native execution, differential/hosted/provider-fsync parity or full P2/P3 claim.
See the
[ledger](sf-v1-port-ledger.md#queue-pressure-and-slow-peer-policy-slice-p3-bounded-f02f03f06f07f08f11)
and [provenance](sf-v1-port-provenance.md#queue-pressure-and-slow-peer-policy-slice-p3-bounded-f02f03f06f07f08f11).

### SF V1 Role-Aware Message Ingress Slice

```bash
cargo nextest run -p kuberic-runtime --lib --all-features --profile ordinary \
  -E 'test(replicator::v1::role::tests::) | test(replicator::v1::secondary::tests::) | test(replicator::v1::primary::tests::) | test(replicator::v1::copy_establishment::tests::) | test(replicator::v1::transport::tests::)' -j 2
```

Cases invoke actual role/primary/secondary/queue/stream/session owners, with a
real tonic connection from primary pressure policy to targeted secondary fault.
Retained ingress observes native initial-progress, provider-epoch, activation,
promotion/demotion, data-loss, direct-Close quorum-prewait and Abort boundaries.
Typed batch validation cannot partially admit malformed input; required service
ACKs remain explicit and framed buffers retain logical accounting.
Atomic-group/non-normal data metadata is an explicit adapter error, not
silently accepted as ordinary byte-stream data.

Accepted replication changes the actual ACK reply identity. RequestAck/context
ACK cannot retarget it; same-epoch copy and targeted InduceFault do not inherit
an extra current-primary gate. Target identity and initialized copy are required.
Future-copy/StartCopy faults and repeated notifications latch one actual
secondary fault and deliver the caller report outside guards. Reporting alone
does not fabricate stream cleanup/EOS, application ACK or membership changes.

Use live nextest inventory for discovery and partition coverage. This is a
private, production-unselected slice, not a new factory or host endpoint.
Production host listener/authentication/registry and partition-report wiring,
broader Open/restart/rebuild, monitoring/health, public adapters and activation
remain pending. No native execution, differential/hosted/provider-fsync parity,
full workspace ordinary execution or PostgreSQL/KinD execution is claimed.
See the [ledger](sf-v1-port-ledger.md#role-aware-message-ingress-slice-p3-bounded-f01f04f05f08f09f11)
and [provenance](sf-v1-port-provenance.md#role-aware-message-ingress-slice-p3-bounded-f01f04f05f08f09f11).

### Built-In Service Operation Delivery

```bash
cargo nextest run -p kuberic-runtime -p kvstore2 -p sqlite-replicated \
  --all-features --profile ordinary --test-threads 1 \
  -E 'test(hosted_operation_stream_) or test(stream_) or test(sqlite_operation_stream_copy_preserves_verified_uncommitted_suffix)'
```

Thirteen hosted factory/service tests hold real copy, copy-complete, active and
retained replication operations. They cover delayed durable ACK, duplicate
commitment, rejection/drop/invalid history, exact retries, cancellation,
authority/role/epoch/configuration fencing, synchronous-effect drainage,
shutdown without consumers, and primary-local no-redelivery. Memory-backed
hosted tests prove sequencing, not fsync; real KV/SQLite consumer tests verify
durable commitment without duplicate application and SQLite cancellation.
Deterministic admission-race tests complement hosted active-effect drainage.
A real SQLite factory/service build preserves an intact uncommitted suffix
over copy, verifies per-frame ACK/retry without reapplication, and prevents
post-commit rejection from supplying successful replay or terminal proof.

### Built-In Read Visibility and Prefix Repair

```bash
cargo nextest run -p kuberic-runtime -p kvstore2 -p kuberic-level-tests \
  --all-features --profile ordinary --test-threads 1 \
  -E 'test(sf01_) or test(sf11_committed_prefix_)'
```

Twelve focused tests cover pre-provider publication failure/drop/revocation,
real normal KV reads behind primary/secondary post-visible apply barriers,
unavailable required quorum, genuine settlement, mutate-then-error, caller
cancellation, terminal authority and equal raw counters without settlement.
The hosted quorum-three fixture injects third-member socket-route failure;
the in-process primary fixture physically cuts both peers' TCP routes.
The exact SF-11 matrix records primary `(11,10)`, peer `(5,5)` replay of
6-11 before suffix commitment and genuine divergent/missing-history rejection.
See the [audit's evidence and scope](service-fabric-contract-audit.md#sf-01-and-sf-11-revalidation);
these tests establish new normal-read admission, not isolation of already
admitted reads or arbitrary application data views.

### Role-Specific Scalar Progress

```bash
cargo nextest run -p kuberic-runtime --all-features --profile ordinary \
  --test-threads 1 \
  -E 'test(sf_d02_) or test(sf11_committed_prefix_) or test(primary_write_) or test(=commit_failure_permanently_contains_the_ambiguous_suffix) or test(=hosted_public_v1_authorized_data_loss_maps_only_the_effective_outcome)'
```

Two new hosted tests query the actual public control and a journalled Progress
outcome while a real peer service ACK is held after local apply, then while local
commit is held after quorum. They cover success, joined caller cancellation,
a completed epoch barrier, secondary pre/post-service-ACK acceptance and gap
rejection. No elapsed-time assertion supplies the safety proof. The existing
SF-11 matrix also queries opened None, secondary-to-primary promotion and all
successful/failed suffix-settlement outcomes. Extended commit-error, close/epoch
and nonempty typed Preserve recovery cases distinguish scalar commitment from
independent report tails and access. These memory-backed tests prove protocol
sequencing, not fsync; existing real-provider SF-01 regressions remain separate.
See [SF-D02 closure](service-fabric-contract-audit.md#sf-d02-primary-progress-promise-exceeds-the-built-in-query-result)
and the [role/native-source qualifications](service-fabric-api-semantics.md#role-specific-progress-and-recovery).

### Quiet Snapshot Boundary Proof

```bash
cargo nextest run -p kuberic-runtime --all-features --profile ordinary \
  --test-threads 1 -E 'test(sf10_)'
```

Five public factory/host tests complete quiet empty/nonzero snapshot-only
Build, activate, and run both catch-up modes without another write or rebuild.
They cover explicit progress and already-known-peer `INVALID_LSN`, including
no retained boundary record on either side. Other cases cover concurrent and
later exact suffixes, conflicting later payload, exact read-only proof retries,
mixed proof/data rejection, wrong receipt/session/configuration/history/storage,
missing live durable progress, and source abort during delayed proof work.
The source-abort barrier delays the outbound proof, not a service operation:
the prefix proof intentionally performs no application mutation.

Disabling only the receipt-proof branch makes both quiet nonzero journeys
fail with `IncompatibleRetainedHistory`; empty cases pass. Memory-backed
fixtures prove protocol sequencing, not fsync. Existing real-provider,
service-ACK/cancellation and SF-01/SF-11/SF-D02 regressions remain independent.
See [SF-10 closure](service-fabric-contract-audit.md#sf-10-quiet-snapshot-builds-fail-boundary-proof)
and [proof semantics](service-fabric-api-semantics.md#completed-snapshot-prefix-proof).

### In-Process Controller/Application Journeys

```bash
cargo nextest run -p kuberic-level-tests --all-features --profile ordinary \
  -E 'test(in_process::journeys)'
```

The [in-process harness](../../proposal/in-process-system-test-harness.md)
connects the production reconciler and Kubernetes adapter to real agent RPCs,
hosted KVStore2 replicas, TCP replication, and durable files. Only Kubernetes
resource storage, Pod/PVC realization, and explicit socket faults are simulated. It requires no cluster,
containers, kubeconfig, or application binaries.

Ordinary-tier journey/assertion tests cover bootstrap/status conflict,
support Service/Secret repair, contained restart, replacement with delayed exact
cleanup, replacement followed by same-storage restart, stopped-primary failover,
live-primary isolation/reconnection, quorum loss/restoration, and spec-driven
switchover. Controller recreation cuts cover persisted intent and durable
completion before status advancement for replacement and failover, plus prepared
and completed switchover handoff. Provider LSNs/snapshots detect duplicate logical
effects; exact accepted completions must be consumed once. Scenarios call
`KvService` directly rather than its HTTP router.

The [downstream harness design](../../proposal/downstream-in-process-testing.md)
is implemented as `kuberic_controller::test_util`, behind the optional
`test-util` feature. The [downstream guide](in-process-testing.md) describes the
application factory, typed controls and cleanup contract. KVStore2 adapters and
application assertions remain in `kuberic-level-tests`; the reusable store,
transport and orchestration live in the controller testing module.

The independent persisted counter application has normal workspace
tests in `kuberic-level-tests::downstream_test_util`, covering its provider and
bootstrap, restart, replacement, failover, live isolation, quorum restoration,
transfer and error/cleanup journeys. They are included in the nextest archive
and ordinary partitions. An ignored Rust source gate compiles those same
self-contained test bodies against independently packaged crates and checks
feature-off/private/conformance boundaries in the separate source-check job.

Readiness requires no pending accepted operation, transition, replacement,
session renewal, or planned transfer; exact admitted/live session, creation and storage agreement;
canonical configuration evidence; and settled access for all live hosts,
including excluded predecessors. Admitted committed and applied progress must
agree. Survivor observations explicitly exclude only
acknowledged isolated hosts. Rejected real application writes, not stale
inaccessible reports, establish stale-writer safety. Timed-out writes remain
ambiguous; restored members must agree on an absent or attempted value.

Owned TCP proxies cut existing duplex streams and new source/destination
connections at both replication and Build dial paths, including controller
control traffic, without replacing the isolated host/service/session. Unrelated
connections survive cuts and restoration. Fresh-Build fault probes verify zero
provider effects while blocked and transport reconnection, not automatic retry
of a failed Build. Negative-path tests drain active cuts and entered barriers.

Switchover cancellation/retargeting after durable peer preparation and controller
recreation must complete the frozen transfer before considering the edited spec.
Completion identifies that original request; both transfers preserve receipts.
An unaccepted request must not block failover when its source is already absent.

Live-primary return is exercised after genuine session renewal both with and
without rejected writes on the isolated host. The clean-tail case must fence and
recover the same host/session through retained-log catch-up, without snapshot
Build or a manufactured ambiguous suffix. Renewal projection cannot override
newer explicit Epoch/Role work or a conflicting same-epoch role.

Each scenario owns dynamically allocated loopback listeners, resources, and
storage. Callers pass a bound control socket at startup, not host construction.
One server task owns it across initialization and established agent serving,
with no close/rebind or shared listener ownership. The replicator binds during
Open and the controller discovers its returned endpoint through the real agent
report. A testing-feature hook routes physical sockets using the real canonical
source/destination identities, without changing commands or protocol payloads.
Teardown joins forwarding tasks, hosts and application consumers before releasing storage;
failures retain the data directory and report its path. Deployment, Service
networking, watch delivery, and process pause/resume remain KinD concerns.
Live network isolation is not scheduler suspension: the retained KinD failover
journey still pauses and resumes the original containerd task.

## Source-Check Tier

```bash
just nextest-source
```

This checkout/build-tools job runs:

- implementation/source-policy and API inventory/privacy checks;
- the ignored executable Public V1 conformance gate;
- the ignored Rust packaged-consumer compilation/feature-boundary gate;
- workspace doctests and rustdoc/API checks.

The conformance gate executes the registered replacement scenarios under one
bounded parent deadline and checks that every KSF mapping selects an existing
test. Behavior and provider cases otherwise execute from ordinary archive
partitions; they are not hidden behind a Python or nested Cargo test runner.
`nextest-build-only` remains a compatibility name for the source-check recipe.

## PostgreSQL

Routine validation:

```bash
just nextest-postgres-smoke
```

The smoke target builds the owned command binary and runs representative
Public V1 custom-Replicator scenarios. Run `just nextest-postgres` only when
the full PostgreSQL suite is explicitly required.

## Archive Reproducibility

```bash
just nextest-archive
just nextest-validate-archive
```

The archive includes every nextest-discoverable test binary from the current
all-feature workspace, including ignored source gates. Archive validation
repeats exact source/ordinary/PostgreSQL/KinD tier and partition accounting
against those packaged binaries.

Source checks execute in their own checkout-backed job with Cargo and build
tools; archive inclusion does not make a source-sensitive test archive-only.
Doctests run explicitly there because nextest archives do not contain them.

## Focused Public V1 Coverage

Important focused targets include:

- `kuberic-controller::public_v1_production`: fresh resource bootstrap and
  preseeded-report ordering, exact replacement-incarnation selection,
  contained restart/session renewal, and failover-delay boundaries;
- `kuberic-runtime::public_v1_wire`: protocol-10 rejection and serialized
  `identity` fields;
- `kuberic-runtime::public_v1_conformance`: KSF catalog and ownership
  mutations;
- `kuberic-runtime::public_v1_recovery`: lifecycle, build, failover,
  switchover, containment, task, and writer behavior;
- `kuberic-runtime::public_v1_session_renewal`: durable stage replay,
  participant alignment, monotonic journal supersession, and exact fencing;
- SQLite/KVStore2 `provider_state_rejection`: complete tree/byte immutability
  for old provider state.

## Owned KinD Scenarios

```bash
export KIND_CLUSTER_NAME=kuberic-level-dev
mkdir -p target/kuberic-level-dev
export KUBECONFIG="$PWD/target/kuberic-level-dev/kubeconfig"
export KUBE_CONTEXT="kind-${KIND_CLUSTER_NAME}"
export KUBERIC_AGENT_BEARER_TOKEN=local-level-triggered-token

just create-kind-cluster
just level-triggered-images
just level-triggered-install
just level-triggered-kind-test bootstrap replacement failover
just delete-kind-cluster
```

The harness requires an explicit ownership receipt and refuses default/shared
cluster coordinates. The scenarios inspect current Public V1 authority,
resources, agent diagnostics, and exactly one granted writer. Bootstrap proves
quorum writes after a contained process restart; replacement proves preserved
data and exact new-incarnation admission; failover uses a sustained containerd
task pause and proves preserved data, post-failover quorum writes, resumed
old-primary fencing, and stale-write rejection. The scenarios do not read
removed legacy status fields.
