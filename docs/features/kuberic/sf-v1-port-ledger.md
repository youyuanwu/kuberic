# SF V1 Port Foundation Ledger

**Current selection:** the connected default-factory cutover composes these
owners through real authenticated listeners, public assignment/ACK streams,
service-owned KV/SQLite commitment and named host/controller observations.
See [Classic V1 Runtime](classic-v1-runtime.md). The slice boundaries below are
historical, not current selection flags or native differential qualification.

**Historical delivery scope: P1 foundation, unselected P2 substrate slices and bounded P3 primary,
secondary, copy, role, data-loss, queue-policy, message-ingress and Open/recovery ownership slices, plus bounded private P4 service, protected-facade execution, host access/lifecycle and real-KV/SQLite commit adapters; no selected replication behavior change.** Native pin:
[`3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d`][native].
This ledger implements the foundation deliverables in the
[faithful-port roadmap](sf-v1-semantic-alignment-plan.md#dependency-ordered-implementation-program).
The removed bespoke engine is the migration/safety baseline, not the native
specification. `v1/` destinations below are under
`kuberic-runtime/src/replicator/`; OperationQueue and the bounded components
described below were implemented without activation at their delivery boundaries. P1 itself implemented
no component.

## Evidence and status

The connected public evidence is
[`classic_v1_production.rs`](../../../kuberic-runtime/tests/classic_v1_production.rs),
including real host/control/peer RPCs, real service files and an
[`independent normal-feature provider`](../../../scripts/fixtures/classic-v1-provider/src/lib.rs).
The public factory no longer selects `replicator/stateless` or the old replication
RPC. Controller and in-process journeys exercise native copy before voting
admission, renewed sessions and staged configurations. These are Rust composition
and application-safety witnesses, not native SF execution or blanket audit closure.

These are the **historical P1 baseline** statuses for the family, contract and
scenario rows below. Only the explicit slice updates following this table
change component status; they do not close multi-family contracts.

| Dimension | P1 status |
|---|---|
| Native production source | source-reviewed at the immutable pin |
| Selected native tests | derived expectations where explicitly named; otherwise production-derived synthetic inputs |
| Port implementation/review | **pending P2+ / no component implementation to review** |
| Native binary/test execution | **not run** |
| Ported component execution / native differential comparison | **not implemented / not run** |
| P1 compiler guards | existing external API fixture pins the protected resolved interfaces; no new replicator behavior tests |
| Current-engine execution | only separately named existing Rust witnesses, below |
| KSF | existing structural/current-contract evidence, not native parity |

This document records **11 families, 15 contracts, 33 acceptance scenarios,
29 capability/settings cases and 8 current-evidence entries**. The scenarios
are source-backed inputs and expected observations for future component tests,
not executable behavior coverage. P2+ tests must invoke the actual ported
queue, sender, session and other components against these expectations.
Tests that only compare these descriptions, constants or locally defined
formulas do not establish replicator correctness and are not retained.

The strengthened existing
[external compiler fixture](../../../kuberic-runtime/tests/fixtures/public_api.rs)
checks real protected-interface compatibility. Existing current-engine witnesses
remain separately identified below; neither those witnesses nor the compiler
fixture establishes native or ported-engine parity.

The [provenance register](sf-v1-port-provenance.md) identifies selected
translations, changes and notices. Source citations below are pinned. `R/`
means `src/prod/src/Reliability/Replication/` in that repository. Sources and
selected native test cases are recorded in the tables below and the provenance
register.

### OperationQueue slice (P2)

F03 now has an internal, synchronous, source-guided
[`OperationQueue`](../../../kuberic-runtime/src/replicator/v1/operation_queue.rs#L212)
with **42 original component tests**, plus three representation-accounting
regressions in the async/tonic slice below
([tests](../../../kuberic-runtime/src/replicator/v1/operation_queue/tests.rs#L73)).
Queue-owned parts of Q1–Q4 and L2 are exercised: four cursors, ordered sparse
admission, duplicates, partial retrieval, positional capacity/resize and byte
limits, retention, callback call-point timing/self-disable/replay, trimming,
rewinds, both suffix discards, classification/full reset and transfer.
In particular, rejected count admission can already remove history; earlier-hole
byte admission can discard a higher uncommitted suffix. Primary LSN assignment
and role-transfer normalization belong to later managers/receivers, not this queue.

Implementation review resolved a conditional-invariant/no-op regression after
non-normalizing transfer; three real regression tests preserve that boundary.
The scoped queue tests run under ordinary and are included by existing archive/
partition rules. The protected positive/negative compiler fixtures still guard
the unchanged resolved aliases and interfaces. The
[testing guide](testing.md#sf-v1-operationqueue-slice) provides the selector and
measured inventory; the [provenance register](sf-v1-port-provenance.md#operationqueue-implementation-slice-p2-not-all-of-p2)
records the selected production/test translation and representation changes.

**Not established by the queue slice:** native execution or differential parity;
sender/session/network ACK/tonic/buffer-adapter work; manager/receiver/provider/role integration;
production selection; full P2, multi-family FR closure or P5 acceptance.
Other families retain pending status except the bounded slices below. Scenario rows remain
source provenance/expectations, not another test-vector implementation.

### Sender Session ACK Slice (P2, Bounded F06)

Private, compiled and **production-unselected** components now implement:

- [`ReliableOperationSender`](../../../kuberic-runtime/src/replicator/v1/sender.rs#L103):
  separate monotone receive/apply cursors, ordered single/replay admission using
  the existing shared queue entries, receive-based retirement, native
  operation-count window/retry rules, timer-only RequestAck, completed-LSN
  side channel and receive/apply latency/decay collection.
- [`AckSender`](../../../kuberic-runtime/src/replicator/v1/ack.rs#L33):
  first-deadline batching, immediate force, pending-only close flush, latest
  owner callback rather than captured payload, construction-time force gate,
  trace/issuance time and advisory timer cancellation.
- A **partial** [`ReplicationSession`](../../../kuberic-runtime/src/replicator/v1/session.rs#L98)
  translates selected RemoteSession/ReplicationSession duties: fixed supplied
  identity, current sending versus original operation epoch, replication and
  RequestAck intent/expiry, progress views and one rooted coalescing ACK
  processor. Ordered handoffs are replication sender/dispatch, copy replication
  gate, conditional initiating primary callback, then copy ACK. Copy terminal
  -1 is absorbing; cached replication pairs may replace one field downward
  while sender high-water marks stay monotone. Close does not drain the
  processor or reject native post-close ACK input.

There are **61 new actual component tests**: 28 sender (including its production
statistics helper), 13 ACK scheduler and 20 session. Combined with the queue,
those four original component families contain **103 tests**. They drive actual components with
manual time and explicit handoff delivery, not formulas defined in tests or an
executable scenario catalog. Reviews corrected ACK trace-stopwatch retention
on close/reopen; a real regression preserves it.

**Adaptation:** short synchronous transitions stage owned batches/callbacks;
all external effects run outside serialization. Selected entries and session
roots can outlive close. Deadlines and timer callback entry are injected seams,
not an integrated timer runtime. Callback false means send-queue-full only;
carrier error/fault mapping is still absent. Safe bounded latency iteration
preserves native positional traversal in its dense valid-input domain; sparse
send ordering is tested, arbitrary sparse latency-cursor parity is not claimed.

**Outside this sender/session slice:** carrier, timer and buffer integration
are recorded separately below. Still unimplemented are primary,
secondary/receiver, CopySender/CopyAsyncOperation/context/establishment and
role integration; provider enumeration, quorum/build completion; native session
slow-fault flags/notification timer, MustCatchup and promotion-owner integration.
These are named native responsibilities, not implicitly successful callbacks.
Copy gate/ACK actions are unresolved owner obligations, not executed copy
algorithms. No task abort substitutes for future advisory cancellation/drain.
This is not a full RemoteSession/ReplicationSession port, full P2, native
execution/differential parity, hosted evidence or multi-family contract closure.

See [actual test selectors](testing.md#sf-v1-sender-session-ack-slice) and the
[selected source/test register](sf-v1-port-provenance.md#sender-session-ack-implementation-slice-p2-bounded-f06).

### Async Tonic Substrate Slice (P2, Bounded F05/F08/F11)

The private [`transport`](../../../kuberic-runtime/src/replicator/v1/transport.rs),
[`buffer adapter`](../../../kuberic-runtime/src/replicator/v1/buffers.rs) and
[`async runtime`](../../../kuberic-runtime/src/replicator/v1/async_runtime.rs)
prepare the language/execution/transport boundary. The private protobuf service
is a logical-message carrier, **not native wire compatibility**; the selected
`ReplicationData` RPCs and host listener remain unchanged.

The carrier retains independent replication/copy/context ACK events, full
endpoint/incarnation routing, separate current-sending and original-operation
epochs, operation metadata/segments, terminal/error fields and reclamation
hints. Bounded send admission, per-message expiry, stream replacement and
observable transport outcomes do not constitute receive/application/quorum
completion. A reconnect is not a new replica incarnation or role transition.
Bound-route validation is not host authentication or native role/epoch admission.

An explicitly chosen raw-single or versioned framed-segments adapter preserves
the protected `Bytes` and boxed byte-stream types. Raw bytes are never sniffed
for framing; empty stream items remain data. Framing preserves zero buffers
versus one empty buffer, empty segments and their order. Malformed frames fail
explicitly, and logical payload accounting remains separate from framing/wire
overhead. Version 1 frames contain `SFVB`, a one-byte version, a little-endian
u32 buffer count, one little-endian u32 length per buffer, then ordered payloads.
The selected policy, never the prefix, determines interpretation.
`QueueOperation::from_data` validates the selected representation and
retains its logical size; queue admission, history, completion, rewinds and
reclamation use that size rather than charging frame headers as service data.
The original raw constructor retains its existing opaque-byte accounting.

Carrier expiry is checked while queued at the transport handoff; it does not
promise to retract an already handed-off HTTP/2 frame. Ingress retains bounded
count/encoded bytes plus at most one bounded staged message. The carrier spawns
no tasks: callers own/poll/join the client driver and server future.

Tokio timers drive the actual session retry and ACK scheduler components, not
reimplemented timing formulas. Short transitions select owned effects, then
callbacks run outside guards. Native cancellation/close does not retract
already-selected work; a pending final ACK is not service drain. Copy/primary
processor actions remain explicit owner handoffs, not fabricated copy/build/
quorum success. Runtime effect staging is bounded at 64 entries, with an ACK
final-close slot reserved; `StagingFull` rejects admission before core mutation.
This adapter bound is separate from native queue/window limits. Explicit
shutdown rejects fresh runtime ingress and joins admitted effects; native close
alone still permits rooted work/ACK processing. Dropping an unjoined runtime is
reported as abnormal abort containment, never successful native drain.

Actual-component and real loopback-tonic selectors are documented in
[testing](testing.md#sf-v1-async-tonic-substrate-slice). The
[provenance register](sf-v1-port-provenance.md#async-tonic-substrate-slice-p2-bounded-f05f08f11)
separates native semantic sources from the Rust-specific framing and carrier.
This slice adds **54 actual tests**: 27 async runtime, 20 carrier, four buffer
adapter and three queue-accounting regressions, for **157 combined v1 tests**.

**Remaining owners:** primary/secondary role admission and epoch policy,
receiver reorder/dispatch/ACK modes, copy/context/provider enumeration and
establishment, quorum/build completion, session slow-fault policy,
MustCatchup/promotion, host credentials/listener/controller integration and
atomic P5 cutover. No full P2, selected engine, native execution/differential,
hosted parity or multi-family contract closure is claimed.

### Primary Queue and Quorum Slice (P3, Bounded F02/F06/F09)

Private
[`ReplicationQueueManager`](../../../kuberic-runtime/src/replicator/v1/replication_queue_manager.rs)
and
[`ReplicaManager`](../../../kuberic-runtime/src/replicator/v1/replica_manager.rs)
extend the existing queue and sessions with a bounded primary ownership core.
They are not a selected primary replicator or the complete native
`ReplicaManager`/`UpdateConfiguration` lifecycle.

Successful enqueue assigns an LSN independently of quorum completion; failed
admission does not consume that LSN. Primary commitment requires configured
remote apply ACKs plus the primary's implicit vote, not a provider-local commit
hook. The actual queue retains distinct issued, committed, completed and
capability boundaries. Normal reclamation uses receive ACKs capped at
commitment, further constrained by idle progress; all-current catch-up uses
apply ACKs instead. An idle cannot supply a quorum vote.

Normal queue progress obeys both current and enabled previous configurations.
Current quorum-one uses the native no-remote sentinel; previous quorum-one
with remote sessions instead constrains progress by their minimum apply ACK.
Missing previous quorum blocks normal commitment, not the independently saved
current-configuration catch-up observation. The configuration catch-up cap is
conditional on `AllowMultipleQuorumSet` being disabled.

Configuration updates capture/raise the issued-tail catch-up boundary B.
Ordinary writes and wait creation do not recapture B. Pending **All** follows
the latest-issued tail and all-current apply progress. **WriteQuorum** compares
current quorum apply Q with B, then requires every marked current session to
have apply progress at least Q, not merely B. A completed wait stays terminal.
Assignment/completion and catch-up receivers are ownership adaptations, not
application transactions or cancellation rollback.

There are **44 new actual component tests**: 13 queue-manager and 31
replica-manager cases, for **201 combined v1 tests**. The integrated runtime
library selection passed 258/258. Tests use real sessions and ordered ACK
handoffs, not a test-local quorum formula or executable copy of this ledger.
Review regressions guard cached CC observations before primary notification,
same-root session reuse, failure-atomic configuration/removal, explicit shutdown
errors after cleanup, and rejection of ahead-of-tail ACKs before admission even
when configured quorum is missing. See the [selector and validation scope](testing.md#sf-v1-primary-queue-and-quorum-slice).

**Owner boundaries remain explicit:**

| Native responsibility | Remaining owner / boundary |
|---|---|
| Session creation, build admission, full native configuration history and routing/role/epoch admission | Future primary/establishment/role owners; this slice consumes concrete established sessions, not host replica descriptors or fabricated build success. |
| Slow active-secondary reserve quorum, operation age, statistical eligibility and queue-pressure recheck (`ReplicaManager.cpp:1871–1981`) | Future primary slow-peer policy. Ordinary session ACK progress does not authorize exclusion from reclamation. |
| Idle target pressure retarget before fault (`ReplicaManager.cpp:1983–2074`) | Future CopySender/build owner plus slow-peer policy; no fixed-snapshot substitute. |
| Fault flag, reliable-sender close, build-completion disable and retrying InduceFault (`ReplicationSession.cpp:127–276`) | Future session fault owner/timer integration. The carrier's InduceFault message alone is not this policy. |
| Idle-to-active promotion flag/latency reset (`ReplicationSession.cpp:279–283`) and secondary-to-primary/demotion/provider drains | Future session/role/receiver owners. Queue-transfer normalization alone is not a functioning role transition. |
| Provider epoch, receiver ACK modes, copy/context/EOF/build completion | Bounded receiver/ACK and epoch handoffs are recorded below; full provider/copy/build/role state machines remain P3. Protected service/host adapters are P4 and atomic activation is P5. |

The [source register](sf-v1-port-provenance.md#primary-queue-and-quorum-slice-p3-bounded-f02f06f09)
separates translated algorithms from these deferred owners. No full P2/P3,
native execution/differential comparison, provider durability, hosted parity
or activation claim follows from this component slice.

### Secondary Receiver and ACK Slice (P3, Bounded F04/F05/F06/F09)

Private
[`SecondaryReceiver`](../../../kuberic-runtime/src/replicator/v1/secondary_receiver.rs),
[`ReceivedOperation`](../../../kuberic-runtime/src/replicator/v1/received_operation.rs)
and the
[`shared operation stream`](../../../kuberic-runtime/src/replicator/v1/operation_stream.rs)
translate selected native receiving-side ownership, not a complete
`SecondaryReplicator`. They remain compiled and production-unselected.

The actual `OperationQueue` orders gaps/duplicates and bounds retained
count/span/logical bytes. Required ACK is `hasPersistedState || RequireServiceAck`:
received/committed progress can precede contiguous service completion.
Volatile default completes on ordered dispatch admission, before dequeue.
Completed-history capability is the first retained completed entry, or zero
without history; it is not the received or copied tail. Batch early-return,
queue-full and primary completed-hint branches remain distinct.

The operation callback has native NotCalled, ReadyToRun, Running, Completed and
Cancelled states. ReadyToRun still blocks queue completion. Real callback entry
releases the queue predicate; owned outside-guard effects finish the callback.
Ordinary unstarted retirement cancels; selected/running work retains its owner.
EOS is distinct and cannot have its pending callback cancelled. Repeated active/
completed ACKs fail explicitly; a cancelled ordinary late ACK is a no-op.
Cleanup preserves the exact selected callback instance even after queue reset;
a retired receiver cannot start a different receiver's callback.

Stream getters share consumption, never replay. Empty open queues wait, graceful
queue close drains to permanent EOF, and explicit EOS remains an operation.
Wakeups and aborted-entry release are staged outside owner guards. Every reader
is blocked behind a popped internal epoch marker until matching completion;
dropping a reader or handoff cannot fabricate provider success.

The fixed copy epoch and moving minimum sending epoch remain separate from
original operation epochs. The first contiguous operation establishes
`min(original, copy)` provider-epoch state. Required-ACK epoch changes wait for
the previous contiguous service-ACK boundary before handing off provider work;
successful completion reinstalls/replays the real queue callback and may
discover another barrier. Optimistic mode stages stream epoch markers while
its receive/completion progress can advance ahead of service consumption.
Provider invocation is not implemented. Failed explicit completion cleans the
receiver, surfaces an error and contains further ingress/dispatch; this is a
Rust adapter failure boundary, **not the full native fault/EOS/drain aftermath**.
Standalone stream abort uses a sticky error once per handle, then terminal EOF.

There are **92 new actual component tests**: 31 received-operation, 22 shared
stream and 39 receiver/integration, for **293 combined v1 tests**. The integrated
runtime library passed 350/350. Actual ACK-scheduler tests sample current receiver
progress at issuance and use the caller-supplied copy pending count in the
pending-ACK threshold. An actual manager/send/receiver/ACK/session trace stops
at the unresolved ordered copy-owner handoff and cannot complete the primary
early. It is not a functioning build, role transition or hosted engine.
See [selectors and validation scope](testing.md#sf-v1-secondary-receiver-and-ack-slice)
and [source/representation provenance](sf-v1-port-provenance.md#secondary-receiver-and-ack-slice-p3-bounded-f04f05f06f09).

**Remaining owners:** actual copy/context/StartCopy establishment and fixed-copy
epoch admission, provider invocation/direct epoch/recovery, native fault reports
and stream-fault/EOS/drain ordering, full secondary Close/promotion/demotion/
queue transfer, primary slow-peer/session policies, facade/host/controller
adapters and atomic cutover. No protected type, selected factory/data path,
dependency or public wire changes. No full P2/P3, native execution/differential,
provider-fsync, hosted parity or activation claim.

### Copy Receivers Slice (P3, Bounded F04/F05/F07)

Private
[`SecondaryCopyReceiver`](../../../kuberic-runtime/src/replicator/v1/secondary_copy_receiver.rs)
and
[`CopyContextReceiver`](../../../kuberic-runtime/src/replicator/v1/copy_context_receiver.rs)
reuse the actual queue, received-operation, shared-stream and ACK-scheduler
components. These are receiving owners, not a working copy/build protocol.

Secondary copy orders gaps and duplicates in a count-bounded queue with no
invented memory bound. Required mode separates received from contiguous
service-completed progress; optimistic mode completes at ordered dispatch
admission, before dequeue. `copyDone` constructs a permanently empty closed
stream without inventing a terminal operation. Getters share one consuming
dispatch queue, and terminal reacquisition never replays.

Numbered network copy/context terminal operations preserve their positive
protocol LSN and native invalid operation epoch `(0,0)`, distinct from a locally
generated zero-LSN EOS. Native transport marks the last operation EOS regardless
of the configured EOS-ACK policy. With that policy enabled, secondary copy
orders/exposes the numbered EOS and required mode waits for its real callback.
Legacy empty last markers consume a protocol LSN but are not service data.
Ordinary empty data is still an item, not EOF. Terminal arrival before a gap,
full-queue retry, duplicates and nonempty legacy test-support last items retain
their separate branches.

Copy receive completion reports `-1` before outstanding service ACKs necessarily
complete; both copy ACK fields become `-1` after the queue is released. These
sentinels do not become replication progress or provider durability. The native
legacy terminal bookkeeping can still require an explicit ignore-EOS-wait
cleanup even when no EOS callback was exposed; the port does not silently
change that drain predicate. Close, update-epoch-failure and stream-report-fault
methods preserve their receiver-local differences. Selected callbacks survive
queue reset, ordinary unstarted callbacks cancel, and EOS callbacks retain
their real roots. A foreign callback cannot release a live queue predicate.

Copy context is received **on the primary from the persisted target**, not
generated from the primary's provider. Its optimistic queue completes without a
service ACK, and its data-only stream hides protocol terminal metadata.
Terminal-before-gap waits for all preceding context; terminal ACK progress
includes the marker's LSN. Real AckSender batching uses the native strict
`completed - lastIssued > maxPending` context threshold, not the replication
threshold. Selected sends sample current progress/error at issuance; a failed
send is not requeued. Successful explicit enumerator close permits an already
pending dequeue to finish but rejects later polls; error close fails readers
and issues the final error ACK. Shared Rust close state, sticky error/EOF and
immediate inaccessible-data retirement are documented ownership adaptations,
not native COM result parity.

All wakeups, transport callbacks, ACK finishes and rejected/retired input-root
release are staged outside receiver serialization. Malformed inputs produce
explicit errors, not EOF or success. A real two-receiver/scheduler witness
exercises the combined pending threshold and samples both progress pairs at
issuance. A real terminal-copy/session trace stops at the unresolved copy-build
handoff; observing terminal ACKs cannot execute a missing sender or build owner.
See [selectors and validation](testing.md#sf-v1-copy-receivers-slice) and
[source/representation provenance](sf-v1-port-provenance.md#copy-receivers-slice-p3-bounded-f04f05f07).

There are **68 new actual tests**: 31 secondary-copy, 33 copy-context, two
numbered-EOS constructor and two pair/session integration cases, for **361
combined v1 tests**. The integrated runtime library passed **418/418**; exact
inventory is 776 = 730 ordinary + 41 PostgreSQL + 3 KinD + 2 build-only.
Inventory discovery is not execution of those tier bodies.

**Remaining owners:** provider invocation and target-context acquisition,
CopyAsyncOperation/CopySender enumeration, reliable sending and build completion/
retargeting, StartCopy/session/epoch/identity admission, cross-receiver fault
reports/EOS/drains and full primary/secondary role transitions. Protected
facade/service/host/controller adapters and atomic activation remain P4/P5.
No protected API, selected production path, dependency, native execution,
differential comparison, provider-fsync or hosted-parity claim is introduced.

### Copy Enumeration Slice (P3, Bounded F05/F07/F11)

Private
[`CopyAsyncOperation`](../../../kuberic-runtime/src/replicator/v1/copy_async_operation.rs)
now owns serial provider-stream enumeration and reliable copy/context sending,
reusing the actual OperationQueue, ReliableOperationSender, buffer adapter and
clock. It does not acquire a provider stream, implement CopySender or build an
idle replica. Existing protected provider types remain unchanged.

Copy sequence numbers start at one. Queue capacity uses the native copy count
settings, clean-on-complete and ignore-commit policy, with no copy memory bound.
A full queue parks exactly one already-numbered operation and stops further
provider polls. Receive ACKs suppress retries; only applied ACKs release queue
space and retry the parked item. ACK entry wakes the executor-driven provider
loop instead of polling it inline. Per-item representation limits are separate
from queue capacity, and framed payload accounting uses logical data length.

Empty data remains data; EOF produces a numbered protocol terminal even for an
empty stream. The last-enumerator callback runs once, outside serialization,
before sender Add and even when the terminal is parked. No poll follows EOF.
Cancellation at a full-queue stall or after EOF completes immediately; otherwise
the driver stops before the next poll or allows an in-flight poll to finish.
A provider error takes precedence over the post-cancellation check.
Non-cancellation provider failures retain their error and `EndGetNext` fault
description. Cancellation and malformed input never become successful EOF.

The actual sender supplies retry windows, one-shot deadlines, RequestAck and
receive/apply progress/averages. Send queue full remains a retryable false
callback, not an ACK. Final copy progress marks `ready_to_complete`, but only an
explicit outer-owner success request completes enumeration; no replication
build gate is invented. Native `End` separately closes the sender idempotently.
ACK input is already normalized: wire `-1` conversion belongs to future
CopySender, not this owner.

Rust replaces Threadpool/Begin-End with a caller-driven Tokio future and weak
handles. A unique stream lease preserves pending polls across timer/wake
branches and dropped drive futures, returning the provider to the callback
root on drive-future drop/exit. Selected sends retain that provider/queue root through
owner release and End. Dropping a driver future or owner is not native
cancellation, successful drain or completion. Polls, callbacks, wakes and final
payload destruction execute outside transition guards; the sender's new
read-only pending view supports lifetime staging, not another ordering queue.

There are **45 new actual tests**, including source-native twelve-item drained
success and fifteen-item/eight-slot/full-retry cancellation vectors, real
executor wake/cancel cases, lifetime/reentry regressions and real copy/context
receiver boundaries. Combined V1 coverage is **406**; the runtime library passed
**463/463**. Exact workspace inventory is **821 = 775 ordinary + 41 PostgreSQL +
3 KinD + 2 build-only**. See [selectors and validation](testing.md#sf-v1-copy-enumeration-slice)
and [source provenance](sf-v1-port-provenance.md#copy-enumeration-slice-p3-bounded-f05f07f11).

**Remaining at this slice boundary:** CopySender final-copy/replication build gates, EOF-issued
target capture and retargeting; provider invocation/context acquisition;
StartCopy identity/session/epoch establishment; cross-receiver faults/drains
and full role orchestration; slow-provider API monitoring/health reporting.
Protected facade/service/host/controller adapters
and atomic activation remain P4/P5. No selected production path, protected API,
dependency, native execution, differential/hosted/provider-fsync or full P2/P3
completion claim is introduced.

### CopySender Build Gates Slice (P3, Bounded F06/F07/F11)

Private
[`CopySender`](../../../kuberic-runtime/src/replicator/v1/copy_sender.rs)
now wraps the actual CopyAsyncOperation with the native
NotStarted/Started/LSNSet/ReplCompleted/Completed state machine. At the real
enumeration EOF callback, a supplied fallible getter captures the latest
**issued** primary replication LSN, not a creation-time or committed snapshot.
The callback runs outside guards, before terminal sender Add, even when EOF is
parked. Lookup failures complete with an explicit error without inventing an
EndGetNext provider fault.

The replication gate does not latch from preterminal replication ACKs.
Final-copy application and replication application through the captured target
are separate conditions; neither alone succeeds. The existing session's ordered
replication-gate, primary-progress and copy-ACK handoffs are consumed by real
component tests without bypassing their continuation. CopySender normalizes
native copy `-1` sentinels to the known terminal; zero still means no update.
Receive progress suppresses retries, applied progress releases the copy queue,
and impossible ACKs for a parked terminal cannot freeze the replication gate.
Numeric terminal ACKs also work. Copy-context mode skips the replication gate
and retains no replication finish target.

Pending replication targets only increase while LSNSet; they cannot regress or
reopen a satisfied gate. A real session/primary-queue witness shows retargeting
increases idle retention progress and releases more queue entries without
declaring build success. This ports the target update, not the full periodic
freshness/queue-pressure/slow-fault policy or idle promotion owner.

Disable-completion blocks new success decisions, not copy queue advancement.
A selected success racing disable is explicitly failed and faulted at End with
the native description. Pre-Begin failure skips enumeration; Begin is one-shot,
external FinishOperation and enumeration cancellation retain their distinct
timing, and first completion wins. End is separate and idempotent. Progress and
averages delegate to the actual sender. Weak handles avoid cycles, and selected
enumeration work roots this callback owner through End/owner release; external
polls/getters, sends, wakes and retiring payloads stay outside transition guards.

There are **47 new actual tests**, **453 combined V1**, and **510/510** passing
runtime library cases, including concurrent retarget/gate and deterministic
disable/success races, real executor wakeups and both secondary service-ACK
modes plus primary context receipt. Exact workspace discovery is
**868 = 822 ordinary + 41 PostgreSQL + 3 KinD + 2 build-only**.
See [selectors and validation](testing.md#sf-v1-copysender-build-gates-slice)
and [source provenance](sf-v1-port-provenance.md#copysender-build-gates-slice-p3-bounded-f06f07f11).

**Remaining owners:** provider invocation/context acquisition, reliable StartCopy
identity/session/epoch establishment, full BuildIdle/session/primary/secondary
orchestration, periodic/slow-peer policy and promotion, cross-receiver faults/
drains and roles, provider monitoring/health reports, and P4/P5 adapters/atomic
activation. No protected interface, selected production path, dependency,
native/differential/hosted/provider-fsync or full P2/P3 completion claim.

### Copy Establishment Slice (P3, Bounded F04/F05/F07/F08/F11)

Private
[`copy_establishment.rs`](../../../kuberic-runtime/src/replicator/v1/copy_establishment.rs)
adds primary and secondary establishment/acquisition owners around the existing
session, receivers and CopySender. The primary creates the incoming persisted
target-context stream before publishing and sending reliable StartCopy.
The actual session sender retries the replication-start operation, samples the
current sending epoch, uses infinite expiration and closes on ordered ACK
processing or explicit cleanup. Already-selected work remains deliverable.
The existing logical SendIntent adapter maps StartCopy onto the private carrier;
one real tonic witness verifies its bound replica/epoch/start fields.

Bound full endpoint/partition/incarnation/address validation precedes admission.
The secondary initializes its actual replication receiver once, ACKs duplicate
StartCopy without reacquiring context, rejects older epochs and explicitly
faults an unexpected higher established epoch rather than silently restarting.
Persisted targets invoke their own protected GetCopyContext; nonpersisted targets
skip context generation. Context enumeration/sending uses the existing
CopySender in no-replication-gate mode and accepts the actual numeric context ACK.

First valid target context establishes the persisted primary even before context
EOF; a successful replication ACK establishes a nonpersisted primary with empty
context. The primary invokes protected GetCopyState with the caller-supplied
frozen classic committed boundary, **replication start minus one**, then begins
the existing CopySender. That boundary is not replaced by the distinct
latest-issued replication completion target captured at actual copy EOF.
Acquisition errors retain explicit GetCopyState/GetCopyContext fault descriptions;
remote errors preserve first completion and close reliable StartCopy retries,
including zero-progress error ACKs.

Uniquely owned acquisition futures survive interrupted driver polls. Cancel
closes incoming context to unblock a provider consuming it and finishes the
existing CopySender; it does not fabricate successful cancellation/drain of an
arbitrarily pending provider callback. Provider calls, sends, wakeups and final
payload releases occur outside short transition guards. Handles are weak;
enumeration callbacks root establishment state without a strong owner cycle.

There are **45 new actual tests**, **498 combined V1**, **555/555** passing runtime
library cases and exact **913 = 867 ordinary + 41 PostgreSQL + 3 KinD + 2 build-only**
workspace discovery. See [validation](testing.md#sf-v1-copy-establishment-slice)
and [provenance](sf-v1-port-provenance.md#copy-establishment-slice-p3-bounded-f04f05f07f08f11).

**Remaining owners:** idle-role/session eligibility and committed-boundary
selection, full BuildIdle/primary/secondary/session lifecycle, cross-receiver
fault/drain/role orchestration, periodic/slow-peer policy and promotion, provider
monitoring/health, and P4/P5 adapters/atomic activation. Caller-owned acquisition,
enumeration, ACK handoffs and timer delivery remain explicit, not success stubs.
No protected API, selected factory/service/wire/host/controller or dependency
change, native/differential/hosted/provider-fsync parity or full P2/P3 claim.

### BuildIdle Lifecycle Slice (P3, Bounded F02/F05/F06/F07/F11)

Private
[`BuildIdle`](../../../kuberic-runtime/src/replicator/v1/build_idle.rs)
now coordinates the existing ReplicaManager, establishment and CopySender
cores rather than supplying another queue, sender or build-success flag.
Admission validates the bound route and manager identity, creates/opens a fresh
idle session with no supplied progress, rejects duplicate membership, and
captures actual pending replication under manager serialization. Classic V1
starts at **max(current committed, previous-configuration quorum floor) + 1**;
the separate V2 copy-until-latest capability is not inferred or implemented.
The frozen provider boundary remains distinct from the actual latest-issued
replication target captured at copy EOF.

Admission registers lifecycle metadata and publishes reliable StartCopy before
any callback can remove the replica. Pending replay and the selected StartCopy
dispatch outside the manager guard, in that order. Already-selected sends may
survive removal; retry selection and build success do not. The driver composes
establishment, one retained GetCopyState acquisition, actual copy enumeration
and End. Ordered ACK processing always delivers replication sends, copy gate,
primary progress and copy ACK, continuing the rooted processor even when a
handoff fails. Impossible copy/replication progress is rejected before sticky
session coalescing, reusing the actual CopySender normalization.

The driver owns pending-build replication/context deadlines and StartCopy
retries after establishment. Ingress and manager enqueue wake it to resample
new deadlines; interrupted polls retain the actual acquisition/stream.
Successful builds remain idle and continue receiving replication. Their
registry samples the live actual CopySender target for retention, keeping the
terminal target after the driver is released. At that slice boundary,
subsequent ordinary session timer/ingress ownership remained pending; the
primary lifecycle slice below now supplies that bounded owner.

Native BuildIdle cancellation cleans incoming context and is checked before
source provider acquisition; it is not session removal or an unconditional
post-acquisition cancellation result. Remove/manager Close separately close
replication and cancel establishment/copy outside guards. Close retires pending
replicate waiters before idle cancellation, never creating a false quorum
success by deleting membership. Old handles cannot cancel a replacement with
the same replica ID. Pending acquisition is not declared drained on removal;
actual provider failure retains its fault and precedence.

BuildEnd records actual GetCopyState/EndGetNext faults, with first private
primary fault retained and subsequent builds rejected. This observation is not
host health reporting. Registry metadata holds weak manager/driver references;
abandoned drivers are explicit errors, not successful cancellation/drain.

There are **41 new actual tests**: 38 BuildIdle and three actual queue snapshot
cases, for **539 combined V1** and **596/596** passing runtime library cases.
Exact workspace discovery is **954 = 908 ordinary + 41 PostgreSQL + 3 KinD +
2 build-only**. See [validation](testing.md#sf-v1-buildidle-lifecycle-slice)
and [source provenance](sf-v1-port-provenance.md#buildidle-lifecycle-slice-p3-bounded-f02f05f06f07f11).
Earlier slice counts/exclusions are historical boundaries.

**Remaining owners:** full primary/secondary/session/role composition,
cross-receiver/provider drains, periodic/slow-peer policy and promotion,
provider monitoring/health and P4/P5 adapters/atomic activation. All components
remain private/unselected. No protected API, selected production wiring,
dependencies, native/differential/hosted/provider-fsync parity, full ordinary
execution or full P2/P3 completion claim is introduced.

### Secondary Lifecycle Slice (P3, Bounded F04/F05/F06/F09/F11)

Private
[`Secondary`](../../../kuberic-runtime/src/replicator/v1/secondary.rs)
now composes the actual secondary copy/replication receivers,
SecondaryEstablishment, context CopySender and AckSender under one explicit
caller-driven lifetime. Establishment shares the same actual replication
receiver used by data ingress and ACK callbacks; there is no second queue or
synthetic initialization/progress ledger. Typed copy/replication ingress assumes
caller-owned route/epoch admission; full role/host demux is not implemented.

Provider UpdateEpoch calls are retained across interrupted polls and run
outside guards. Full ACK barriers publish their provider obligation before
ACK-work completion can reenter Close. Success resumes the actual queue
callback, or discards noncompleted entries while closing. Full-barrier provider
failure performs both receivers' cleanup and fails pending drain; stream-marker
provider failure instead records the actual fault and resumes dequeue, matching
the separate native aftermath. Context acquisition and actual reliable context
sending reuse the existing establishment/enumeration owners; no source-local
context, empty-success provider fallback or synthetic callback completion exists.

Legacy ordinary Close aborts dispatch, cleans operation queues and waits
already-reserved provider epoch calls, **not held data ACKs or GetCopyContext**.
EOS-enabled ordinary Close clears unpumped replication before EOS when copy
is incomplete, closes admission, drains initialized copy then replication and
required service/EOS ACKs, then waits context acquisition and provider epoch
work. Uninitialized streams use native abort bypass. Cross-receiver stream
fault cleanup ignores data ACKs but preserves EOS ACK waits; a failed copy
drain returns its error without waiting replication/provider drainage. Abort
separately contains dispatch and waits actual pending context/epoch calls,
even following a failed Close. These are not universal graceful-drain promises.
No path claims arbitrary in-flight provider GetNext drainage.

Shared dispatch observation distinguishes initialization, empty-open/drained
queues, blocked epoch markers and aborted queues. Dequeue/transition wakes and
new ACK deadlines resample continuously pinned drivers. ACK sends snapshot
actual progress outside owner guards. Weak handles explicitly reject abandoned
owners; cleanup callback lifetime tokens retain the actual provider without a
root/receiver cycle. No autonomous task or production owner is introduced.

There are **41 new actual tests**: 39 secondary owner and two dispatch lifecycle
cases. V1 is **580**, runtime **637/637**, unchanged non-V1 **57**. Exact
discovery is **995 = 949 ordinary + 41 PostgreSQL + 3 KinD + 2 build-only**.
See [validation](testing.md#sf-v1-secondary-lifecycle-slice) and
[source provenance](sf-v1-port-provenance.md#secondary-lifecycle-slice-p3-bounded-f04f05f06f09f11).

**Remaining owners at that slice:** full primary/session/role composition, externally
requested epoch reconfiguration, promotion/queue transfer, periodic/slow-peer
policy, provider monitoring/host health and P4/P5 adapters/atomic activation.
All components remain private/unselected. No protected API, selected production
wiring, dependencies, native execution, differential/hosted/provider-fsync
parity, full ordinary execution or full P2/P3 completion claim is introduced.

### Primary Lifecycle Slice (P3, Bounded F02/F06/F07/F08/F09/F11)

Private [`Primary`](../../../kuberic-runtime/src/replicator/v1/primary.rs)
composes the actual ReplicaManager, bound sessions and retained BuildIdle
drivers under one explicit caller-driven lifetime. Builds are published before
reentrant pending-replay/StartCopy sends. Existing provider futures and
CopySender/context owners survive interrupted polls; no alternate queue,
provider acquisition, sender algorithm or ACK arithmetic is introduced.

While BuildIdle owns build timers, the primary drives ordinary replication
retry/RequestAck timers for terminal builds and caller-established sessions.
Bound endpoint/incarnation/address lookup precedes ACK/context admission.
Idle sessions receive copy/context handoffs; active sessions receive
replication-only ACK processing. Shared delivery preserves the actual rooted
sender -> copy replication gate -> primary quorum progress -> idle copy ACK
order, including reentrant/coalesced ACKs and selected work after Close.
At that slice boundary configuration remained the caller-established seam, not
native UpdateConfiguration/session creation or a claim of promotion.

Ordinary Close optionally prewaits actual pending replicate quorum completion.
Timeout continues Close without fabricated ACKs or a timeout-shaped Close
failure. Actual manager Close cancels remaining writes/catchup and builds,
closes senders, then selected synchronous effect/oneshot deliveries drain.
Client waiter-body consumption is not native callback dispatch and is not
awaited. Abort skips/preempts the prewait and has an independent abnormal
outcome. Pending provider build calls are retained, not silently declared
drained; explicit continued build driving can await their actual callbacks.
Weak ingress reports abandonment, not successful Close.

There are **42 new actual tests**, including finished-build retransmission and
actual subsequent quorum completion, finished-idle nonvoting behavior,
receive/apply and RequestAck, shared CC/PC timers, queue-full retry, stale
identity, removal/replacement, interrupted acquisition, actual provider errors,
reentrant callbacks, selected-delivery drainage and finite/infinite quorum
prewait/Abort. Native CC/PC lookup takes precedence over retained idle membership.
V1 is **622**, runtime **679/679**, non-V1 unchanged **57**.
Discovery is **1037 = 991 ordinary + 41 PostgreSQL + 3 KinD + 2 build-only**.
See [validation](testing.md#sf-v1-primary-lifecycle-slice) and
[source provenance](sf-v1-port-provenance.md#primary-lifecycle-slice-p3-bounded-f02f06f07f08f09f11).

**Remaining owners at that slice boundary:** native configuration/session
creation, role and external epoch orchestration, promotion/drained queue transfer,
periodic/slow-peer/MustCatchup policy, monitoring/host health and P4/P5
adapters/atomic activation. Protected interfaces, selected wiring,
dependencies/tier policy remain unchanged. No native binary/differential/
hosted/provider-fsync parity, full ordinary execution or full P2/P3 claim.

### Primary Configuration Slice (P3, Bounded F02/F06/F08/F09)

Private [`PrimaryHandle`](../../../kuberic-runtime/src/replicator/v1/primary.rs)
now accepts typed replica descriptors through distinct catch-up and current-only
configuration entry points. It creates/opens actual sessions using supplied
progress and the owner's sender settings/callback, and reuses actual
current/previous/idle roots. The existing manager remains authoritative for
quorum, captured catchup boundary, marks, replay, queue reclamation and waiter
completion. No parallel configuration arithmetic or synthetic session is added.

Native configuration matching is **replica-ID based**, unlike bound ACK ingress.
Reused descriptors do not overwrite an existing session's endpoint/incarnation,
callback or ACK progress. Full route/identity checks remain on incoming ACKs.
Unknown current progress requires existing current or idle membership;
previous-only membership does not suffice. New current members must have retained replay;
missing replay for new previous-only members explicitly pauses their senders.
Existing PC contracts rather than expands; CC/PC overlap shares one root.
Bindings are published before outside-guard replay can reenter.

Actual idle membership is distinguished from active-priority ACK classification.
Idle-to-active **session promotion**, including a retained PC/idle overlap,
preserves sender/build roots, sets the native promotion marker and resets actual
sender averages once. Live latency watches remain. Promotion neither declares
copy success nor invents cancellation/drain of a pending provider/build; the
actual copy owner and explicit cleanup remain authoritative. This is not
secondary-to-primary role promotion.

There are **34 new actual-component tests** for creation/seed/replay, identity/
progress reuse, unknown admission, CC/PC overlap/contraction/retirement,
invalid-input atomicity, real idle promotion and averages, pending provider
retention, quorum/catchup reevaluation, reentrant ACK/reconfiguration/Abort,
selected-send Close drainage, queue-full retry and pinned deadlines.
V1 **656**, runtime **713/713**, non-V1 unchanged **57**. Exact discovery:
**1071 = 1025 ordinary + 41 PostgreSQL + 3 KinD + 2 build-only**.
See [validation](testing.md#sf-v1-primary-configuration-slice) and
[source provenance](sf-v1-port-provenance.md#primary-configuration-slice-p3-bounded-f02f06f08f09).

**Remaining owners at that slice boundary:** role/external-epoch orchestration, secondary-to-primary
promotion/drained queue transfer, periodic/slow-peer policy, monitoring/host health
and P4/P5 adapters/atomic activation. The established-session seam remains for
isolated use, not a requirement of the new descriptor API. Protected interfaces,
selected wiring, dependencies and tier policy are unchanged. No native execution,
differential/hosted/provider-fsync parity, full ordinary execution or full P2/P3
completion claim. Earlier omissions/counts are historical.

### Role Transition Slice (P3, Bounded F01/F02/F04/F09/F11)

Private [`RoleReplicator`](../../../kuberic-runtime/src/replicator/v1/role.rs)
now serializes native role edges and external epoch requests above the actual
primary and secondary owners. Initial primary creation uses supplied cached
progress or the actual provider query, then completes the provider epoch before
publishing successful role completion. Cancelled polling retains the selected
operation/future; no autonomous task or public factory is introduced.

Secondary-to-primary promotion uses its own Close/drain branch, not ordinary
Close. Required-service-ACK and optimistic dispatch-consumption modes remain
distinct, as do legacy incomplete-copy rejection and EOS fully-received-copy/
data/EOS drainage. Existing provider epoch calls survive the transition.
Promotion transfers actual retained queue entries and original operation epochs
into the existing queue normalization, not a queue rebuilt from scalar progress.
The new primary has no established quorum until configured.

Primary-to-secondary demotion cancels pending writes, samples final actual
quorum progress, rejects uncommitted suffixes, trims retained count/logical bytes
and transfers actual queue ownership. The receiver completes already-applied
history without dispatching it again, and enforces the service's ACK mode for
newly received data. Source handles close admission; transfer is one-shot.
Old-primary build providers stay rooted for explicit later build drainage,
not falsely included in role completion.

Primary external epochs use issued tail rather than commitment, notify actual
sessions and call the provider even on duplicates; retries continue while the
callback is pending. Secondary direct external epochs instead raise minimum
admission, discard out-of-order work, reject unfinished copy and drain dispatch/
service ACKs without an extra provider callback. Stream-induced provider epochs
keep their existing distinct ownership. Provider/build faults prevent successful
role handoff; explicit Abort retains already-selected provider obligations.

Actual-component regressions exercise role transitions, secondary drainage/
epochs and queue normalization. Use the live inventory validator for current
test discovery and partitioning. See
[validation](testing.md#sf-v1-role-transition-slice) and
[provenance](sf-v1-port-provenance.md#role-transition-slice-p3-bounded-f01f02f04f09f11).

**Remaining owners at that slice boundary:** data-loss/recovery/reset orchestration,
periodic/slow-peer/MustCatchup policy, full host role/epoch demux,
monitoring/health, P4 public adapters/outer lifecycle and P5 atomic activation.
All components remain private/unselected. No protected API, selected wiring,
dependency/tier change, full ordinary execution, native/differential/hosted/
provider-fsync parity or full P2/P3 completion claim.

### Data Loss Recovery Slice (P3, Bounded F01/F02/F03/F09/F11)

Private [`RoleReplicator`](../../../kuberic-runtime/src/replicator/v1/role.rs)
now selects primary-only data-loss checking and retains the actual
`StateProvider::on_data_loss` callback across interrupted polling.
Unchanged state preserves the queue, pending writes, original epochs and
cached progress without querying provider progress. Changed state queries
`last_committed_lsn`, updates cached recovery progress before reset and forwards
through the actual Primary and ReplicaManager into the existing queue reset.
Successful reset clears old entries/original epochs and resets assignment and
previous-configuration catch-up/quorum cursors to recovered progress.
Higher, lower and zero progress are valid; malformed progress is an error.

CheckingDataLoss gates role/epoch requests, previously issued primary handles
and new ACK/context ingress. Existing selected activities remain rooted,
and actual session/build timers continue while the provider is pending.
Reset requires no pending queue/write work and no selected primary activity.
An invalid reset does not cancel writes, synthesize ACKs or silently discard
uncompleted history. Session/configuration roots are not rebuilt or rewritten;
the caller still owns subsequent configuration and replica rebuild policy.

An explicit End result retains both the provider's changed flag and completion
error. Ordinary callback/query/reset failures restore primary admission rather
than applying the generic role-operation fault transition. If restoration also
fails, the original callback/reset error wins. Abort retains the selected
recovery chain, including a changed-only progress query, but cannot reset the
closed primary or publish successful recovery; provider failure remains explicit.

Actual-owner regressions cover admission, retained callbacks, reentrancy,
unchanged operation identity/retries, recovered progress/assignment, reset
preconditions, original errors/state restoration and abnormal shutdown. See
[validation](testing.md#sf-v1-data-loss-recovery-slice) and
[source provenance](sf-v1-port-provenance.md#data-loss-recovery-slice-p3-bounded-f01f02f03f09f11).
Use the live nextest inventory for discovery and partitioning.

**Remaining owners at that slice boundary:** broader Open/restart/host recovery and rebuild policy,
periodic/slow-peer/MustCatchup policy, full host role/epoch demux, monitoring/
health, P4 public/outer lifecycle adapters and P5 atomic activation.
All components remain private/unselected. No protected API, selected wiring,
dependency/tier change, native/differential/hosted/provider-fsync parity,
full ordinary execution or complete P2/P3 claim.

### Queue Pressure and Slow-Peer Policy Slice (P3, Bounded F02/F03/F06/F07/F08/F11)

The private [`primary`](../../../kuberic-runtime/src/replicator/v1/primary.rs)
now drives the actual
[`manager policy`](../../../kuberic-runtime/src/replicator/v1/replica_manager/policy.rs)
after queue admission, including rejected full admission, and owns periodic
freshness ticks. Freshness raises pending idle copy targets to commitment;
pressure first raises them to latest-issued, updates actual progress and
rechecks utilization before faulting. Neither path resets a completed copy gate.
Dynamic private settings are sampled on each decision; persisted/volatile
restart defaults remain distinct. No public settings adapter is activated.

Utilization is the truncated maximum of pending LSN-span and logical-byte
percentages, excluding completed history. Oldest-operation age uses Tokio's
monotonic clock; queue transfer refreshes actual operation timestamps as native
does. Active faulting requires spare current members, write quorum above one,
the age guard, current quorum strictly beyond the queue head and a receive
latency above the fastest remote quorum's mean plus twice population deviation.
The duration outlier must also be the least-receive-progress replica.

Idle faulting uses the actual BuildIdle/CopySender completion-disable operation.
A completed build wins that race and is not faulted. Successful slow faulting
closes the reliable replication sender **before reclamation**, then removes its
receive constraint without fabricating receive/apply ACKs or deleting membership.
Active quorum/all-applied and the live MustCatchup minimum still include a
slow-faulted active replica; late ACKs cannot reopen its closed sender. Such a
MustCatchup wait can remain pending until the host changes configuration.
Faulted idle-to-current promotion is rejected; previous-config overlap is not
promotion.

Session-owned InduceFault notifications retry independently of replication,
even after send-queue backpressure or accepted delivery. Active notification
starts after the retry interval; idle notification waits for **both** terminal
copy ACK sentinels. Build or terminal-session drivers own the deadlines, and
Close/Abort/removal cancels them. Selected effects dispatch outside owner guards.
The typed tonic adapter uses the bound route's actual target replica/incarnation,
not the session's legacy logical incarnation. Receipt is not a remote fault ACK.

Actual-owner regressions cover strict freshness/outlier/age/usage boundaries,
retarget-before-fault reclamation, full-admission policy, completion races,
MustCatchup/quorum/all preservation, retry/identity, reentrancy and shutdown.
Missing actual copy ownership fails explicitly rather than supplying success;
a policy failure never hides an already assigned write. See
[validation](testing.md#sf-v1-queue-pressure-and-slow-peer-policy-slice) and
[provenance](sf-v1-port-provenance.md#queue-pressure-and-slow-peer-policy-slice-p3-bounded-f02f03f06f07f08f11).
Use the live nextest inventory, not a static coverage total.

**Remaining owners:** broader Open/restart/host recovery and rebuild policy,
full host role/epoch/InduceFault demux and remote fault handling, monitoring/
health, public settings/service/outer-lifecycle adapters and atomic activation.
Protected interfaces, selected wiring, dependencies and tier policy remain
unchanged. No full P2/P3, native execution, differential/hosted/provider-fsync
parity or full ordinary execution is claimed.

### Role-Aware Message Ingress Slice (P3, Bounded F01/F04/F05/F08/F09/F11)

The private [`message ingress`](../../../kuberic-runtime/src/replicator/v1/ingress.rs)
now selects actual primary/secondary owners through the existing
[`role owner`](../../../kuberic-runtime/src/replicator/v1/role.rs).
Retained ingress handles observe changing admission; handlers and caller
callbacks execute outside the selection guard. Initial progress acquisition,
promotion/demotion drainage, data loss, role-None teardown and Abort reject fresh
messages. Primary creation admits ACK/context after queue ownership exists,
including while the provider epoch callback is pending; idle-to-active
activation retains secondary admission. Direct primary Close preserves ACK/
context admission during its existing quorum prewait, then closes ingress.

The actual [`secondary ingress`](../../../kuberic-runtime/src/replicator/v1/secondary/ingress.rs)
decodes complete replication batches into the existing received-operation/queue/
stream owners before admission. Buffer policy remains explicit, original and
sending epochs stay separate, and framed overhead is not logical payload.
Copy uses its fixed established epoch and real legacy/EOS terminal handling.
Stale messages are observable typed drops; malformed messages return errors.
The current protected byte-stream adapter admits normal data and copy/context
terminals; atomic-group/non-normal data metadata fails explicitly rather than
being silently erased. Broader metadata capabilities remain an adapter blocker.
Unexpected newer StartCopy/copy epochs report a sticky actual secondary fault.

Accepted replication updates the actual reply route; ACK/context callbacks
receive the selected endpoint/incarnation/address. RequestAck and context ACK
require the established current primary, while same-epoch copy does not impose
an extra sender constraint. Targeted InduceFault checks local replica/incarnation
and established copy state, **not an invented current-primary or epoch gate**.
It latches the first secondary fault and invokes the required caller reporting
handoff outside guards. Duplicates do not report again. Reporting alone neither
cleans streams, generates EOS/ACKs nor removes configuration membership.

Actual-owner regressions and a real tonic journey connect primary pressure
policy and session notification to target fault admission. See the
[selector](testing.md#sf-v1-role-aware-message-ingress-slice) and
[source/substitution register](sf-v1-port-provenance.md#role-aware-message-ingress-slice-p3-bounded-f01f04f05f08f09f11).
No static coverage total is maintained.

**Remaining owners:** production host authentication/listener/registry and
partition-report adapter wiring, broader Open/restart/host recovery and rebuild,
monitoring/health, public settings/service adapters and atomic activation.
The required private reporting callback is not production partition integration.
Protected interfaces, selected wiring, dependencies and tier policy are unchanged.
No native execution, differential/hosted/provider-fsync parity or full P3 closure
is claimed. Historical omissions above are superseded only by this bounded slice.

### Open and Restart Recovery Slice (P3, Bounded F01/F04/F07/F08/F09/F11)

The private [`role owner`](../../../kuberic-runtime/src/replicator/v1/role.rs)
now starts in Created and explicitly Opens once, registering its actual
[`message ingress`](../../../kuberic-runtime/src/replicator/v1/ingress.rs)
in a private logical endpoint registry. Open returns the bound address and full
partition/replica/incarnation identity; it does **not** call the provider or
create a primary/secondary. Duplicate registration fails without replacing the
owner. Registry selection releases its guard before dispatch/callbacks.
This typed in-process registration is not production listener, authentication,
endpoint-string interoperability or host-registry activation.

Native recovery is lazy: an explicitly driven Opened progress query reads the
protected provider callback once, retains it across interrupted runs and caches
success for subsequent queries/initial-primary creation. Query failure returns
the original error without faulting Opened or caching a value; it can be retried.
Initial-primary failure still faults without publishing a primary. Every
successful negative provider LSN normalizes to zero in query, initial-primary
and changed-data-loss recovery, matching the native proxy. Errors are never
normalized into success; an overflowing queue seed still fails explicitly.
The private query lane serializes with role/Close operations rather than
claiming complete native concurrent-query interleavings.

Direct Close keeps registration through the actual primary quorum prewait,
then unregisters on completion including child failure. Role None alone retains
registration until Close. Abort unregisters and seals retained ingress while
the explicit driver retains selected provider obligations; late completion
cannot create a primary or publish successful recovery. Registration drop also
seals old ingress and releases only that endpoint.

The [actual-owner regressions](../../../kuberic-runtime/src/replicator/v1/role/tests/open.rs)
restart with a fresh incarnation and provider-owned progress, not an engine
snapshot/suffix. A lagging current replica fails with the existing explicit
missing-replay error. Caller-directed BuildIdle then uses actual target context,
provider copy enumeration, target service-ACK/copy streams and source build gates
before configuration promotion and new replication/quorum completion.
Old-incarnation delivery is unregistered, and no pre-restart bytes are invented.
The provider implements only the protected StateProvider contract, not
DurableState or a durable-progress oracle. This is an in-memory recovery witness,
not provider-fsync evidence or automatic placement/rebuild policy.

See the [selector](testing.md#sf-v1-open-and-restart-recovery-slice) and
[source/substitution register](sf-v1-port-provenance.md#open-and-restart-recovery-slice-p3-bounded-f01f04f07f08f09f11).
Use the live nextest inventory rather than a static coverage total.
Remaining owners include production host lifecycle/listener/authentication/
registry and partition reporting, automatic rebuild/placement orchestration,
monitoring/health, public service/settings adapters and atomic activation.
Protected interfaces, selected wiring, dependencies and tier policy are unchanged.
No full P3, native execution, differential/hosted parity or provider-fsync
qualification is claimed. Earlier slice-boundary omissions are superseded only
for this bounded private lifecycle and caller-directed recovery handoff.

### Private Service Adapter Slice (P4, Bounded F01/F02/F04/F05/F09/F11)

The private [`service adapter`](../../../kuberic-runtime/src/replicator/v1/service_adapter.rs)
composes the existing role, primary, secondary, queue, build and provider owners.
Its caller-driven async control owner exposes Open, role/epoch changes, progress,
data loss, configuration/build/catch-up/removal and Close/Abort. Dropping a
selected control future retains the actual work; explicit `resume` finishes it
without restarting provider callbacks. The exclusive control lane and separate
steady `drive` are Rust execution substitutions, not full native concurrent
facade interleavings. No autonomous engine/task or public factory is installed.

A cloneable weak service handle selects the current role under a short guard,
then invokes the actual owner unlocked. Payload validation and an injected
write-access callback precede primary admission. That callback preserves native
ReconfigurationPending/NoWriteQuorum/NotPrimary precedence but is not a host
status calculation or serving grant. Successful admission returns the actual
LSN and a separate resumable quorum-completion waiter. Dropping the waiter does
not retract the queue entry; neither assignment nor quorum completion commits
service state, changes provider progress or produces client success.

Copy/replication adapters consume the existing shared receiver queues. Internal
epoch markers hand off to the real secondary/provider driver; the service sees
operations only after that barrier. Operations expose immutable data/LSN/epoch,
optional EOS and parameterless service ACK through the original callback owner.
Reacquisition does not rewind; empty open streams wait; legacy error-then-EOF,
copy EOF, explicit EOS/ACK drainage, duplicate and retired ACK behavior remain
distinct. Build waiters observe actual End via outside-guard notification;
waiter drop is not native Build cancellation or RemoveReplica. Abandoning the
actual driver wakes outstanding build waiters with an explicit owner-dropped
error, not a fabricated End or an indefinite wait. Ordinary Close
and explicit retired-build drainage remain separate. Drop is abnormal abort
containment, not successful callback/provider drainage.

Opened capability is zero and progress uses existing lazy provider recovery.
Stable primary queries read native queue commitment/capability. Secondary
queries read actual receiver progress; querying during copy/uninitialized state
returns NotReady and reports a sticky fault instead of synthesizing progress.
No new query value is fed into existing published host reports.

The [actual-owner regressions](../../../kuberic-runtime/src/replicator/v1/service_adapter/tests.rs)
include helper-free target-context/copy/build, configuration, concurrent
assignment, held service ACK/quorum completion and separate ordered local
commit/client completion. Failed or delayed local commit leaves the service
value invisible even after quorum success. Additional vectors cover interrupted
control/provider/stream work, access/admission failures, volatile implicit ACK,
role handoffs, build cancellation/removal and Close/Abort/abandonment.
See the [selector](testing.md#sf-v1-private-service-adapter-slice) and
[source register](sf-v1-port-provenance.md#private-service-adapter-slice-p4-bounded-f01f02f04f05f09f11).

This is a bounded private P4 preparation, not implementation of the published
facade or full P4/P3 closure. Protected interfaces, production factory, service
writes, selected wire, host/schema/controller reports, dependencies and tier
policy are unchanged. Production host/authentication/listener/registry and
partition reporting, real KV/SQLite migration, settings/capabilities,
monitoring/health and P5 atomic cutover remain pending. No native execution,
hosted/differential parity or provider-fsync qualification is claimed.

### Private Protected-Facade Execution Slice (P4, Bounded F01/F02/F09/F11)

The private [`facade bridge`](../../../kuberic-runtime/src/replicator/v1/service_adapter/facade.rs)
implements the unchanged `Replicator` and `PrimaryReplicator` traits over the
existing service adapter and actual V1 owners. No public constructor or factory
selects it. Open returns the existing logical registration address, not an
authenticated production listener. Exact public identity/process/storage/address
bindings are supplied as a private snapshot; opaque public string IDs are not
hashed or truncated into native sender incarnations.

A bounded request channel selects existing owner transitions. One retained
Tokio task progresses control callbacks, steady timers/ingress/provider barriers
and independent build/catch-up waits. Dropping a trait-call future does not
retract a sent request or destroy selected work. Incompatible controls fail
explicitly; stable primary queries remain available during an external epoch
callback. The active owner future remains alive between wakes. Retired primary
build drivers and outstanding secondary context callbacks transfer intact to
the same task and progress independently after role handoff, not only at final
shutdown. Descriptor conversion preserves former-primary roles in the previous
configuration while requiring the local primary in the current configuration.

Abort synchronously revokes facade/service admission, disables projected ingress
and aborts the projected child outside the projection guard. The task then
performs actual role Abort and observes retained callbacks. This scheduling and
the existing exclusive control lane are explicit Rust substitutions, not full
native concurrent facade/Abort interleaving parity. Reentrant provider Abort
cannot publish late control success. Ordinary Close completes at the actual
role Close boundary; explicit cancellation-safe join additionally observes
provider/build drainage (including context acquisition after legacy secondary
Close) and task errors. Drop is abnormal Abort containment,
not proof of successful Close/join. Channel loss, callback failure and worker
panic are errors, never successful operation results.

[Actual-owner trait-object regressions](../../../kuberic-runtime/src/replicator/v1/service_adapter/tests/facade.rs)
cover interrupted callbacks, query/control admission, real context/copy/build,
configuration/quorum, sender retries, stream epoch/EOS barriers, retired build
progress and distinct Close/Abort/join lifetimes. The protected `Result<bool>`
data-loss call returns an error on failed End; the richer changed/error result
remains in the private actual owner.

See the [selector](testing.md#sf-v1-private-protected-facade-execution-slice)
and [source register](sf-v1-port-provenance.md#private-protected-facade-execution-slice-p4-bounded-f01f02f09f11).
Protected signatures, published assignment/ACK APIs, default factory, services,
selected wire, report/schema/controller/election/rejoin semantics, dependencies
and tier policy remain unchanged. Real-service and authenticated host/status/
reporting adapters, full P4 closure and atomic P5 activation remain pending.
No native execution, hosted/differential parity or provider-fsync qualification
is claimed.

### Private Host Access and Lifecycle Slice (P4, Bounded F01/F09/F11)

The private [`host adapter`](../../../kuberic-runtime/src/host/v1_lifecycle.rs)
composes the existing facade, actual role/primary/secondary owners and an
already-opened `StatefulServiceReplica`. It ports reachable native FUP
lifecycle/reconfiguration access decisions and their selector, not an
applied/committed equality gate. Native TryAgain maps to the existing
`ReconfigurationPending`. Stable classic secondaries deny reads and writes;
promotion retains staged TryAgain/read-granted outcomes; same-primary
reconfiguration retains reads. Demotion retains reads during catch-up, then
the native selector moves directly to TransitioningRole/Completed (denied):
its table's CatchupCompleted row is not selected by that production path.

Host intent/stage and bound remote `Up`/`Ready` membership are explicit private
inputs. Actual replicator/service roles and catch-up outcomes come from retained
callbacks, including successful service addresses. The native access quorum
counts local membership plus CC remote primary/secondary and PC remote
secondary, checks CC minimum and the relevant majority/majorities, and requires
an opened replicator. It is not configured engine W, ACK progress, provider
durability or client commitment. Exact peer identity/process/storage/address
matching reuses the facade binding helper. Authority, fault and retirement
fences remain independent. All service-handle clones, including ones obtained
before host attachment, use the same rebound access seam; callback invocation
remains outside its short guard.

Host/proxy policy and application/outer-teardown orchestration belong to
`host/v1_lifecycle.rs`, not replication-engine internals. The replicator supplies
an opaque unopened-facade setup, liveness/Abort observation and neutral lifecycle
callbacks; the host supplies access policy. No concrete host type is stored or
imported by the engine. Private queue/role/configuration/error types remain
hidden behind this crate-private seam. Cross-layer regression journeys stay
beside existing private core fixtures rather than widening core visibility.

Host controls are caller-driven, retain their actual continuation on waiter
cancellation, and require `resume` to finish host bookkeeping. The existing
facade task independently retains selected engine work; no second replication
executor or algorithm is installed. Successful non-primary role changes clear
host configuration membership. Outer Close revokes access first, aborts
mismatched roles, otherwise closes replicator then application, and records
child errors/Abort explicitly while allowing native outer completion. Repeated
Close observes the cached outcome without replaying callbacks. Cancellation
retains application Close; external Abort suppresses late host success without
hiding completed child diagnostics. Explicit join rejects an outstanding host
continuation and separately reports real retained provider drainage/worker
failure; clean Abort drainage does not re-label a failed child Close as graceful.

[Actual-owner regressions](../../../kuberic-runtime/src/replicator/v1/service_adapter/tests/host.rs)
exercise these boundaries through real context/copy/build, service ACK,
quorum/catch-up, demotion/promotion, held provider/application callbacks,
service-isolated visibility, exact membership errors, worker panic and legacy
context drainage. See the [selector](testing.md#sf-v1-private-host-access-and-lifecycle-slice)
and [source register](sf-v1-port-provenance.md#private-host-access-and-lifecycle-slice-p4-bounded-f01f09f11).

This is not RA/FM/placement, public application Open/factory integration,
authenticated listener/registry or a production status change. Host activation
acknowledgement is not an engine configuration/ACK call; production report,
schema, controller, election and rejoin meanings are unchanged. Real KV/SQLite
migration, service-qualified native serving grants, full P4 closure and atomic
P5 activation remain pending. No native execution, hosted/differential parity
or provider-fsync qualification is claimed.

### Private Real KV Commit Slice (P4, Bounded F02/F04/F05/F07/F09/F11)

The unselected [`KV committer`](../../../examples/kvstore2/src/v1.rs) owns
application preparation, FIFO local commit, committed read snapshots and client
completion. It reuses [`KvPersistence`](../../../examples/kvstore2/src/persistence.rs)'s
actual file/fsync/rename primitive and mutation reconstruction. The implementation
belongs to the service, not the replication engine. The existing `testing`
feature exposes a repository-only byte/LSN/future bridge; a dev-only dependency
lets [private V1 integration tests](../../../kuberic-runtime/src/replicator/v1/service_adapter/tests/kv.rs)
exercise those real service/storage owners without publishing engine types,
changing protected traits or adding a factory/selector.

Source guidance is the existing F02 assignment/completion, F04/F05 receiver
ACK/stream and F07 copy/provider contracts, plus the application example
[`Store/ReplicatedStore.TransactionReplicator.cpp:606-796,848-900`](https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Store/ReplicatedStore.TransactionReplicator.cpp#L606-L900)
at the immutable native pin. The latter supplies service-side ordered local
commit and client-completion evidence, not a full KVS translation or a universal
engine persistence requirement. This KV service chooses serial FIFO commits;
native KVS batching/implicit completion optimizations are not ported.

Assignment is supplied by the actual V1 handle and precedes separate quorum
completion. Durable prepares do not become client-visible KV values.
The retained KV worker waits for quorum, commits locally in assignment order,
and only then publishes client success. Cancelling a wait or dropping a client
handle does not retract engine admission or service work. Native host reads
can remain Granted while KV reads expose only the locally committed prefix.
Service faults independently deny access, notify the supplied fault sink outside
guards and remain observable at cancellation-safe, repeatable drainage.
Abort suppresses not-yet-selected local success, not already admitted replication.

Persisted secondary mutation and complete snapshot installation finish storage
before their no-argument native service ACKs. Source copy acquisition waits for
the requested local committed prefix instead of copying unresolved prepares.
Copy EOF, native build completion and local snapshot installation stay distinct.
The example encodes one versioned whole-snapshot operation; malformed, regressing
or conflicting snapshots fail explicitly. New-instance recovery validates
contiguous local state and discards only locally uncommitted prepares; this is
not same-lifetime rollback, queue recovery, proof of no remote acceptance, or
qualification of old production-store migration.

Actual-owner journeys cover delayed ACK versus received progress, quorum ahead
of local commit, dropped/cancelled client waits, retained Close and explicit
Abort, source-copy waiting, persisted-secondary failure without ACK, native
EOS-enabled stream fault, real-file restart, copied-prefix promotion and later
assignment. Ordinary service tests additionally cover out-of-order completions,
admission/prepare/commit failures, corrupt recovery and worker panic. Use the
[live-inventory selector](testing.md#sf-v1-private-real-kv-commit-slice) and
[source register](sf-v1-port-provenance.md#private-real-kv-commit-slice-p4-bounded-f02f04f05f07f09f11).

Published KV service/factory/HTTP routes, public assignment/ACK APIs, production
host/listener/registry/status/wire/reports/schema/controller/election/rejoin and
protected interfaces remain unchanged. Existing supported KV journeys continue
to use the old selected engine; unreadable state is explicitly rejected rather
than treated as a fresh empty store. Private SQLite preparation is recorded
below; production service migration/serving qualification, full P4 closure and atomic P5 activation remain
pending. File-backed tests are not power-loss qualification, native execution,
hosted parity or differential evidence. No production dependency or tier-policy
change is introduced.

### Private Real SQLite Publication Slice (P4, Bounded F02/F04/F05/F07/F09/F11)

The private [`SQLite companion`](../../../examples/sqlite/src/v1.rs) composes the
existing real SQLite connection, WAL staging VFS and fsynced frame-log storage.
It is application adaptation, not an SF SQLite implementation or another
replication algorithm. A repository-only runtime dev dependency and the existing
example `testing` feature expose neutral byte/LSN/future/IO callbacks to the
[actual-owner fixtures](../../../kuberic-runtime/src/replicator/v1/service_adapter/tests/sqlite.rs),
not private engine types or a public factory.

A retained FIFO SQL worker owns each blocking SQLite transaction independently
of client waiters. The real VFS captures complete WAL frames, obtains actual
native assignment, durably prepares the frames, awaits native quorum completion
and commits the service-local frame log before releasing WAL publication.
Client success is selected only after SQLite commits and the application
confirms publication. LSN assignment, secondary persistence, ACK/quorum, local
commit, WAL publication and client completion are observed at separate cuts.
The existing SQL connection authorizer prevents bypass through attached
databases, journal changes or explicit client transactions. Reads during a
retained transaction return an explicit busy error, not speculative SQL data.
Once drained, queries use the committed, serialized real connection; even a
query-opened connection retains the guarded VFS for subsequent writes.

Definitive SQL/admission rejection does not claim assignment. Post-admission
prepare, quorum, commit or publication failure is ambiguous, fences serving
and reports once outside service-state guards. No engine admission or quorum
is retracted. Abort revokes final success, wakes retained waits and makes no
rollback promise. Close separately observes retained SQL drainage and caches
its outcome across cancelled/repeated waits. Callback/future panics are
contained at the SQLite C-ABI boundary and converted to explicit terminal
faults, not empty/successful results; fault-reporter panic is diagnostic too.

The service persists secondary frames before native parameterless ACK.
It installs a complete versioned snapshot before copy ACK and requires distinct
copy EOF before promotion. Installation rejects malformed, regressing,
conflicting and invalid SQLite images, including actual SQLite page/quick-check
validation before durable manifest publication. Source copy waits for local
commitment, not engine quorum alone. Recovery validates retained records before
discarding only locally uncommitted prepares, then rematerializes committed SQL.
Real-file restart/copy and native receiver-to-primary promotion preserve SQL
and use the actual inherited next assignment. This is not same-lifetime
rollback, proof of no remote acceptance or old-store rolling migration.

The source-guided application ordering uses the same pinned KVS transaction
ranges as the preceding KV slice; native assignment/ACK/copy/role algorithms
remain unchanged. Native host-grant and real filesystem-failure journeys prove
that quorum can remain ahead of local commit without client success; the
EOS-enabled secondary failure path withholds ACK and uses existing native
stream-fault/Abort semantics. See the
[selector](testing.md#sf-v1-private-real-sqlite-publication-slice) and
[source register](sf-v1-port-provenance.md#private-real-sqlite-publication-slice-p4-bounded-f02f04f05f07f09f11).

Production SQLite/KV services, public APIs/protected traits, selected factory,
host/status/listener/registry/wire/reports/schema/controller/election/rejoin and
tier policy remain unchanged. Public service migration/serving qualification,
full P4 closure and atomic P5 activation remain pending. No native binary,
hosted/differential parity, kernel power-loss/fsync qualification or new
production dependency is claimed.

## Source families, transitions and substitutions

| ID / native family and source | Transitions/responsibility | Rust destination (only explicit slices implemented) | Allowed representation substitution | Scenarios |
|---|---|---|---|---|
| F01 facade/factory/ReplicatorState ([state:39–126][state], [QI:12–35][qi]) | initial/transitional/primary/secondary/faulted/closed; role admission | bounded `v1/role.rs`, private `service_adapter.rs`/`service_adapter/facade.rs`, and separate `host/v1_lifecycle.rs`; public selection/factory and production host integration pending | COM/refcounts → owned Rust; actual role/recovery, retained protected-trait driver and host-owned caller-driven outer lifecycle with explicit diagnostics, not public activation | A1, C1, L1, L3 |
| F02 PrimaryReplicator/ReplicaManager/ReplicationQueueManager ([manager:349–421][manager], [queue-manager:261–408][qm]) | CC/PC/idle sessions; enqueue/assignment/completion; quorum/config capture | bounded `v1/replication_queue_manager.rs`, `replica_manager.rs`, `build_idle.rs`, `primary.rs`, `role.rs` | owned futures replace Begin/End, not primary ACK, configured W or B/Q/M arithmetic; classic idle lifecycle, lasting primary/session ownership, descriptor configuration and role/epoch queue handoff delivered | A1, M1–M5, E1 |
| F03 OperationQueue ([queue:677–737,990–1047][queue], [heads:62–87][qh]) | gap/order, retention, count/byte admission, commit/complete/trim/reset/transfer | `v1/operation_queue.rs` | owned storage instead of ring layout, not a plain channel or merged cursors | Q1–Q4, L2 |
| F04 SecondaryReplicator/receivers ([secondary:165–193,951–1003][secondary], [receiver:632–740][receiver]) | reorder/dispatch/completion, fixed copy/minimum replication epoch, drains | bounded `v1/secondary_receiver.rs`, `secondary_copy_receiver.rs`, `copy_establishment.rs`, `secondary.rs`, `role.rs` | async queues/executor; required/optimistic ACK, shared StartCopy receiver, provider callbacks, ordinary Close/Abort, promotion drainage, direct epochs and actual queue handoff delivered; host demux pending | S1–S3, E2 |
| F05 operation/data/stream/provider ([stream:24–80][copy-receiver], [callback:167–224][callback], [IDL:636–683][idl]) | shared getters, data/EOF/EOS, late callback states, provider enumeration | existing protected facade + `v1/buffers.rs`, `received_operation.rs`, `operation_stream.rs`, bounded copy/context streams, `copy_async_operation.rs`, `copy_establishment.rs`, private `service_adapter.rs` | protected Bytes/boxed stream/i64 unchanged; private service streams/ACK and provider-barrier handoff compose actual owners; published facade/full provider lifecycle pending | D1, S2–S3, C1–C2 |
| F06 ReplicationSession/RemoteSession/ReliableOperationSender/AckSender ([sender:548–755][sender], [ACK order:585–627][remote], [batch:64–112,174–208][ack]) | incarnation, separate receive/apply, retry/window/RequestAck, final ACK | `v1/session.rs`, `sender.rs`, `ack.rs`, bounded `primary.rs`, `replica_manager/policy.rs` | injected clocks, lasting session timers/ordered ACKs and slow-fault notification/reclamation policy; quorum/all/MustCatchup remain separate | R1–R3, T2 |
| F07 CopySender/CopyAsyncOperation/context/establishment ([copy:171–254][copy], [build:111–166][build], [retarget:524–579][copy-sender]) | target context, serial polls, one parked item, EOF target and pending retarget | bounded `v1/copy_context_receiver.rs`, `copy_async_operation.rs`, `copy_sender.rs`, `copy_establishment.rs`, `build_idle.rs`; full role composition pending | context acquisition/receipt, reliable StartCopy, serial enumeration/sending, EOF-issued target, copy/replication gates/pending retarget and idle lifecycle delivered; no fixed snapshot-only build substitute | C2–C5 |
| F08 transport/demux/headers ([send:880–937][transport], [session lookup:632–683][primary], [header:25–42][header]) | logical messages, full identity, two epochs, sequence spaces, expiry | `v1/transport.rs`, `ingress.rs`, bounded role/primary/secondary owners, private `sf_v1.proto` | typed role demux, native secondary per-message admission, current reply routing and targeted fault reporting; accepted send is not service ACK; production host integration and native wire compatibility are not claimed | T1–T2, D1 |
| F09 provider epoch/recovery/RA boundary ([epoch:31–90][epoch], [reset:82–126][loss], [status:34–145][status]) | issued epoch boundary, queue recovery/transfer, lifecycle access/direct vs outer Close | bounded `v1/role.rs`, `primary.rs`, `replica_manager.rs`, `replication_queue_manager.rs`, separate `host/v1_lifecycle.rs`; production host/controller integration pending | provider recovery/queue handoff plus host-owned private native access selector/availability quorum and distinct outer Close diagnostics; no RA/FM/placement port; optional provider durability is not native queue progress | E1–E2, L2–L3, H1–H2 |
| F10 settings/actual extensions/partition reports ([settings:490–532,762–808][com], [public defaults:140–177][defaults], [partition:208–350][partition]) | effective settings/update validation, persisted declaration, faults/load/move/health | existing facade/settings/partition adapters, P4/P7 | Rust types; preserve public defaults, unsupported results and credential enforcement | C1, C6, H3 |
| F11 async lifetime/diagnostics ([drain:56–102][drain], [health:62–125][health]) | retain owners through cancel/drain, outside-lock dispatch, batched/cleared warnings | Rust async ownership/clock + host reporting | no Common/Transport/KTL/Boost copy; preserve algorithm-affecting policy, not platform plumbing | R3, C3, H3 |

## Contract rows

These stable FR IDs retain the requirements of the approved earlier
specification; the public roadmap expresses them narratively rather than as
numbered rows. Meanings are included here so no ignored artifact is required.
Each row inherits the source/destination/substitution and pending/run status of
its named families, not an assertion that the contract is implemented.

| Contract / native target | Family → destination and substitution rule above | Concrete scenarios / current evidence |
|---|---|---|
| FR-001 successful admission/LSN separate from completion; advisory cancellation | F02/F11 (owned futures, not rollback) | A1, C3; held primary ACK/commit witness |
| FR-002 service-owned preparation, ordered local commit, visibility/client reply | F02/F09 (queue ≠ provider persistence) | A1; current primary-local commit counterexample |
| FR-003 persisted/required vs volatile implicit ACK; contiguous completion/late scope | F03/F04/F06 (separate cursors) | Q1, S1, S3, R1; held secondary ACK witness |
| FR-004 target context/EOF/receiver lifetime/captured and retargeted build gates | F05/F07 (shared receiver/serial provider) | S2, C2–C5, L1 |
| FR-005 role progress/retained capability/query/reset/epoch | F03/F09 (distinct heads and provider evidence) | Q3–Q4, L2, E1–E2 |
| FR-006 live All, captured WriteQuorum, successor and CC/PC queue constraints | F02 (exact equations) | M1–M5 |
| FR-007 classic access/callback phases; service isolation before native grants | F01/F09 (host adapter, not RA port) | H1–H2, L1 |
| FR-008 direct vs outer Close/error/Abort/cancel/retirement | F05/F09/F11 (owned drainage) | S3, C3, L3; current outer Close witness |
| FR-009 persisted declaration/settings/specific quorum/fault/EOS/scoped reports | F10 (effective defaults/validation preserved) | C1, C6, H3; cases below |
| FR-010 identity/history/provider guarantees; no substitute semantic gates | F08/F09 (tonic fencing + separate evidence) | T1, L2; helper-free hosted service remains P5 |
| FR-011 protected resolved interfaces and lossless buffer representation | F05/F08 (Bytes/boxed Result<Bytes>/i64 fixed) | D1; external compiler fixture and private codec/accounting exercised; service activation pending |
| FR-012 retain historical audit evidence; no closure from planning/ledger | F02/F04/F09 (current vs target separate) | A1, S1, L3 counterexamples; audit remains 7 fixed/14 open + SF-D01; historical SF-D02 fixed |
| FR-013 faithful source-guided complete responsibility port | F01–F11 (all substitutions bounded) | all 33 inventoried vectors; components pending |
| FR-014 Rust async/tonic retain lifetime/order/retry/epochs/ACK/errors | F06/F08/F11 (execution/carrier only) | R1–R3, T1–T2, D1 |
| FR-015 provenance and actual native exposure, not every SDK declaration | F05/F10 (facade boundary) | C1, C6, H3; selected-material register |

## Deterministic scenario inventory

**D** = selected native test assertions translated/simplified; **S** =
production-source-derived small scenario. D does not mean the entire upstream
test/harness was ported. All are source-reviewed expected vectors; sources and
named native cases are preserved in this ledger and the provenance table.

| ID / derivation | Fixed input/actions → expected native observations | Native evidence |
|---|---|---|
| A1 D/S | progress10; enqueue11 accepted with quorum held → assigned11 before completion; failed full enqueue12 leaves next12. Secondary Begin S_OK / End NotPrimary. | [queue-manager:369–408][qm], [PublicApi.Test:454–504][api-test] |
| Q1 D/S | fixed receive2,1 then ACK2,1 → contiguous receive0,2; completed0 until ACK1, then2; no gap skip. Reduced from randomized test. | [queue:677–737,990–1047][queue], [queue tests:1049–1095][queue-test] |
| Q2 D/S | start10, initial4/max8/memory600; four100-byte items, complete one retained → bytes400/retained100; add301 rejected without mutation; add100 →500. Source-derived neighbor: add300 admitted after evicting completed100 →600 (sufficient count growth). | [queue tests:435–461][queue-test], [queue:256–318][queue] |
| Q3 D/S | complete before commit false, after commit true; separately progress100/no retained history → capability0. | [queue tests:607–615][queue-test], [heads:76–87][qh] |
| Q4 D/S | commit1..8, history4; complete0,1,4,5,8 → first completed sequence0,1,1,2,5; tail8. Duplicate does not grow count/bytes. | [queue tests:867–901][queue-test], [queue:371–399][queue] |
| M1 D | W2 remotes[5,6] → quorum6/reclaim5; W3 [2,3,5] →3/2. Too few configured replicas stays pending. | [manager tests:154–220][manager-test] |
| M2 S | All T12/Acc11 false; Acc12 true; another evaluation after T13/Acc12 false until13. Completed waits do not reopen. | [catch-up:43–59][catchup] |
| M3 S | B10/Qcc11/M10 false despite M≥B; M11 true; unmarked only Qcc≥B with enough replicas. Ordinary tail12 does not recapture B. | [catch-up:62–104][catchup], [capture:261–272][qm] |
| M4 S | CC W1/no remote candidate normalized to tail12; PC W1/lone remote7 constrains normal commitment7. | [manager:2471–2524][manager], [queue-manager:319–357][qm] |
| M5 S | B10, candidates commit12/complete11; catch-up-config and AllowMultipleQuorumSet=false cap10/10; true/inactive removes this cap. Missing PC blocks. Configuration at tail13 captures B13. | [manager:1278–1333,2328–2362][manager] |
| S1 S | (persisted,require) FF implicit, FT/TF/TT explicit; persisted receive1 gives current1/complete0 before service ACK; implicit completes at dispatch before dequeue. | [secondary:165–193][secondary], [receiver:177–202,632–740][receiver] |
| S2 S | two getters share consumed queue; terminal reacquisition stays terminal; demotion copyDone gives closed empty copy. | [copy receiver:24–80][copy-receiver] |
| S3 S | retire normal NotCalled callback cancels it; late ACK cannot advance retired receiver; repeated completed ACK InvalidOperation; EOS callback differs. | [callback:167–224][callback] |
| R1 D | add1,2,3; ACK(receive,apply) (1,0),(2,0); add4; (2,1),(3,2),(4,4) → pending[2,3],[3],[3,4],[4],[] and timer off. | [sender test:102–143][sender-test] |
| R2 S | active sender window8/max8/noAckSinceLastCallback=true; retry→4; advancing receive ACK doubles4→8; duplicate leaves8; receive4/apply2/no pending send→RequestAck, not quorum. | [sender:548–579,636–679,732–755][sender] |
| R3 S | combined ACK effects ordered sender progress/dispatch → build gate → progress callback → copy ACK; batch15ms<retry5s; Close flushes only a pending ACK batch. | [remote:585–611][remote], [ACK:64–127,174–208][ack] |
| C1 S | actual QI/settings/fault/exclusion outcomes in next table, not SDK assumptions. | [QI:12–35][qi], [Com:1002–1050][com], [stream:66–85][stream] |
| C2 S | provider [empty bytes,x,EOF] →3 polls,2 items,1 terminal EOF, no poll4; persisted target context to source, volatile omits callback. | [enum:108–121,182–195][enumerator], [copy:171–211][copy], [secondary:982–1003][secondary] |
| C3 D/S | native15/15/4 cancel-at12 case reduced to full queue + one parked item; no additional poll until room/termination; cancel is OperationCanceled, not EOF. Copy count64/1024; no maximum-memory limit. | [copy test:418–449][copy-test], [copy:42–49,242–254][copy], [copy receiver:388–395][copy-receiver] |
| C4 S | prefix10/EOF-issued12; final-copy ACK and apply11 pending, apply12 satisfies replication gate; quiet baseline10/tail10 needs no invented op10 record. | [build:111–144][build], [copy sender:524–579][copy-sender] |
| C5 S | pending LSNSet target12; freshness committed13→13; pressure issued15→15; lower/equal refused; ReplCompleted cannot rise. | [copy sender:555–579][copy-sender], [manager:745–774,1983–2022][manager] |
| C6 S | public defaults/update cases below; retry5s→6s specified public update InvalidArgument; legacy initial128 sets both roles, memory600 without legacy max sets counts0/0 vs max1024 flag gives1024/1024; later EX3 overrides per role before comparison. Credentials validated/updated. Config-store dynamic updates differ. | [Com:490–582,639–808][com], [defaults:136–207][defaults], [public config:12–17][reconfig] |
| T1 D/S | wrong endpoint drops; same replica wrong incarnation cannot update valid session; matching identity still needs role/epoch checks. Sockets/sleep removed. | [transport test:594–617][transport-test], [primary:632–683][primary] |
| T2 S | repl/copy expiry5s; StartCopy/RequestAck unbounded; queue-full stops batch; too-large faults; accepted send≠ACK; slow idle notification retries. | [session:217–243,364–384,423–463][session], [remote:757–790][remote] |
| D1 S | [],[empty],[empty,empty],[ab,c],[a,bc] distinct; empty item≠EOF; sending vs original epochs and receive/apply/copy/context sequences distinct. Private codec/carrier tests now exercise these; service integration pending. | [IDL:636–683][idl], [header:25–42][header], [ACK body:17–24][ack-body] |
| L1 S | Opened→Primary initial; Primary→Active demotes; Idle→Active retains secondary; Active→Idle rejected; None≠initial Opened. | [state:39–126][state] |
| L2 S | promotion transfers completed retained state only; demotion requires tail==commit/trims; changed provider progress7 resets next8/history, unchanged does not reset. | [queue-manager:58–160][qm], [loss:82–126][loss] |
| L3 S | direct child Close failure gives error/fault; outer host aborts/releases child and can complete Success, retaining diagnostic failure (not successful graceful child drain). | [direct Close:134–192][close], [outer Close:209–276][outer-close] |
| E1 S | tail12/commit10 → previous-epoch boundary12; duplicate primary epoch still calls provider; older epoch fails. | [manager:871–920][manager], [epoch:31–90][epoch] |
| E2 S | existing StartCopy(1,2): stale(1,1) drops, higher(1,3) faults receiver, not fresh streams. | [secondary:951–1003][secondary] |
| H1 S | stable secondary NotPrimary/NotPrimary; ready primary Granted/quorum-dependent; same-primary reconfig preserves read; demotion during/after catch-up Granted/TryAgain then TryAgain/TryAgain. | [status:34–145][status] |
| H2 S | promotion transition/catch-up TryAgain/TryAgain; catch-up completed Granted/TryAgain; completed Granted/quorum-dependent. | [status:58–71,101–114][status] |
| H3 D/S | schedule Temp1 then Temp2; explicit timer event emits Temp2 once; Close clears warning with OK, total2. Acceptance≠platform convergence. | [health test:107–135][health-test], [health:81–125][health] |

## Actual exposure and deterministic settings cases

These are source-derived **expected cases**, not implemented Kuberic settings.
Each maps to a concrete scenario above and remains pending port/run status.
All settings fields in the [base/EX1–EX4 public structs:2910–2960][types] are
included; internal policy is explicitly separate.

| Case → scenario | Fixed classification/default/update outcome | Evidence |
|---|---|---|
| settings2 → C1 | QI present; get returns merged effective settings/address/security | [QI][qi], [Com:801–808][com] |
| stream2 → C1 | QI present even with fault/EOS mode disabled | [stream QI:12–24][stream-qi] |
| specific-quorum → C1/M3 | public marker present; successor compares M≥Qcc | [QI][qi], [manager:1092–1099][manager] |
| atomic-group → C1 | absent from ComReplicator; separate ServiceGroup wrappers excluded | [QI][qi], [ServiceGroup:153–180][service-group] |
| internal batch/reservation → C1 | advertised internally; methods and batch stream E_NOTIMPL | [Com:1002–1034][com], [stream COM:48–64][com-stream] |
| internal queue counters → C1 | implemented internal query, not scalar/platform parity | [Com:1036–1073][com] |
| managed buffers → D1 | null Buffer or Count0→E_POINTER; constructor uses Buffer+Offset without an Offset bounds check here; delegate ordinary BeginReplicate | [Com:53–83,1094–1118][com] |
| V2 copy-latest → C1 | separate internal provider marker; not public specific-quorum or classic build target | [provider probe:19–26][provider], [V2 QI:10–29][v2] |
| stream fault → C1 | invalid kind→InvalidArgument before policy; disabled→InvalidOperation | [COM stream:67–78][com-stream], [stream:66–85][stream] |
| explicit EOS ACK → C2 | ordinary null EOF vs enabled explicit EOS requiring ACK | [get-operation:116–136][get-operation] |
| persisted state → S1 | true forces explicit service ACK even require=false | [secondary:165–193][secondary] |
| fault/load/move-cost/scoped health → H3 | reporting acceptance does not prove immediate platform convergence | [partition:208–350][partition] |
| retry → C6 | 5000ms; specified6000 public update rejected; unspecified/equal not this error | [defaults][defaults], [Com:490–532][com] |
| ACK batch → R3 | 15ms, strictly below retry5000ms | [defaults][defaults], [ACK][ack] |
| require ACK → S1 | false; volatile FF implicit, persisted TF explicit | [defaults][defaults], [secondary][secondary] |
| copy queue → C3 | initial64/max1024 count limits; no maximum-memory limit | [defaults][defaults], [copy:42–49][copy], [copy receiver:388–395][copy-receiver] |
| legacy InitialReplicationQueueSize → C6 | default64; specified128 sets both effective roles; later EX3 primary64 overrides primary only to64/128 | [defaults:136–139][defaults], [Com:550–582,639–762][com] |
| legacy MaxReplicationQueueSize → C6 | default1024; explicit1024 sets both effective counts; EX3 can override per role | [defaults:136–139][defaults], [Com:550–582,639–762][com] |
| legacy MaxReplicationQueueMemorySize → C6 | default0; specified600 without legacy maximum-count flag gives effective counts0/0; with max1024 gives1024/1024, before EX3 overrides and validation/comparison, not a successful live-upgrade claim | [Com:550–582,639–762][com] |
| primary queue → Q2 | initial64/max1024/memory0; retained bytes count | [defaults][defaults], [queue test][queue-test] |
| secondary queue → Q4 | initial64/max2048/memory0; history trims independently | [defaults][defaults], [queue test][queue-test] |
| message size → T2 | max52,428,800 bytes; too-large faults | [defaults][defaults], [session][session] |
| clear ACKed → Q4 | false public default; capability not latest progress | [defaults][defaults], [heads][qh] |
| EOS/fault mode → C6 | public false vs internal macro true | [public config:12–17][reconfig], [defaults:185–207][defaults] |
| pending quorum Close → C6 | public timeout0; no invented universal finite drain guarantee | [defaults][defaults] |
| addresses → C6 | localhost:0; listen/publish not compared as breaking fields | [defaults][defaults], [Com:514–515][com] |
| security → C6 | credential changes validated/updated, never ignored; not permission for arbitrary timer upgrades | [Com:762–785][com] |
| internal policy → M5/C5 | AllowMultipleQuorumSet=true; idle freshness60s, driven by explicit tick/pressure in future tests | [defaults:165–177][defaults] |
| max pending ACK → R3 | 32 default; config-store dynamic notifications≠arbitrary public upgrade | [defaults][defaults], [settings test:706–795][settings-test] |

## Current-engine counterexamples and existing witnesses

These preserve existing assertions. They do **not** turn current-engine behavior
into the native specification. Exact commands/results belong to the
[testing guide](testing.md#sf-v1-p1-foundation); source-only entries are not
executed observations.

| Native expectation | Current source / exact Rust witness | Evidence class |
|---|---|---|
| A1/E1 assignment and issued epoch separate from provider commit | `public_v1_recovery::read_fencing::sf_d02_primary_scalar_waits_for_quorum_and_local_commit_across_cancel_and_epoch`: peer ACK held→scalar0; quorum then local commit held→result/scalar pending0; after commit→1. Canceled/epoch case records provider tail0 vs report(0,1). [source:17–135][read-fencing] | actual existing engine witness; not native run |
| S1 persisted received progress before service ACK | `public_v1_recovery::read_fencing::sf_d02_secondary_scalar_is_contiguous_service_acknowledged_not_committed`: apply1/held service ACK→scalar0, after ACK→1; gap3 rejected. [source:138–168][read-fencing] | actual existing engine witness; not native run |
| L3 outer containment can succeed despite child Close failure | `public_v1_recovery::hosted_public_v1_contained_close_restarts_without_replaying_interrupted_children` expects outer error and records ordered Abort. [source:3523–3608][recovery] | actual existing engine witness; KSF-14 unchanged |
| Q3 retained capability | [current engine:2997–3008][current] returns committed tail, not retained floor; at committed100/no history native expects0 | source-reviewed numeric counterexample only |
| M2/M3 catch-up equations | [current engine:3031–3126][current] waits at local committed target with PC/CC/union checks | source-reviewed counterexample only |
| C2 target context | [current engine:1955–1964][current] calls source provider context | source-reviewed counterexample only |
| S2 shared/terminal streams | [current stream:315–365][current-stream] uses single-take getters; CopyComplete is not stream EOF | source-reviewed counterexample only |
| H1 secondary access | [current host:545–605][current-host] grants ready ActiveSecondary reads behind provider equality | source-reviewed counterexample only |

These sources are pinned to the P1 starting commit
`aa6c84224437d7e1ba6c5ad3d227f9426026f616`. Audit statuses stay unchanged:
7 fixed / 14 open plus SF-D01; SF-D02 historically fixed. Neither this ledger
nor its structural checks close findings or establish full optional-mode,
hosted, native differential or provider-fsync parity.

## Next boundary

P2 ports the queue/session/sender/ACK/async/tonic substrate **without production
activation**; remaining session fault/promotion owners stay explicit. P3 has a
bounded primary queue/quorum, replication/copy receiver, context-receipt and
copy/context enumeration/reliable-send and CopySender build-gate/EOF-issued
target/pending-retarget cores, bounded provider/context acquisition and
reliable StartCopy/session/epoch/identity establishment, and the bounded
BuildIdle/idle-session lifecycle: admission/frozen-boundary/pending replication,
ordered orchestration, removal/cancellation and completion cleanup.
The bounded secondary lifetime now composes the actual receivers,
establishment/context/ACK and provider epoch owners, including cross-receiver
faults and ordinary legacy/EOS Close/drain/Abort. The bounded primary lifetime
now retains BuildIdle drivers, session timers/ordered ACK ingress beyond build
completion, quorum prewait and ordinary Close/Abort effect drainage.
Descriptor-based catch-up/current configuration now creates/reuses real sessions,
preserves identity/progress, owns idle-to-active session promotion and composes
native replay/retirement with existing quorum/catchup. The bounded role owner
now adds role/external-epoch sequencing, ACK-mode-specific promotion drains and
actual retained-queue/original-epoch handoff in both directions. Bounded
data-loss orchestration now retains changed/unchanged provider recovery,
actual queue reset and native admission/error/state restoration.
Private Created/Open, lazy normalized/cached initial progress and caller-directed
restart-to-copy recovery now compose actual owners. Broader production host
recovery and automatic rebuild/placement policy remain pending.
Bounded periodic/pressure/slow-peer policy now drives actual retarget/disable,
reclamation exclusion and retrying fault notifications while preserving
quorum/all/MustCatchup. Bounded role-aware ingress now routes real messages,
updates secondary reply identity and reports targeted remote faults through the
actual secondary owner. Production listener/authentication/registry and partition
reporting integration, plus platform monitoring, remain explicit host boundaries.
P4 now has a bounded private caller-driven service/control adapter, separate
assignment/quorum completion, shared stream/ACK/provider-barrier adapters and
actual-owner helper-free service isolation evidence. A private protected-trait
bridge now owns autonomous active/retired progress and distinct Close/Abort/join
lifetimes. A private caller-driven host adapter now composes native access
selection, independent host-availability quorum, actual role/catch-up/application
callbacks and outer Close diagnostics, without activating serving grants.
A private real-KV committer now adds service-owned durable prepares, ordered
local commit/client success, committed read isolation and copy/restart/promotion
evidence over the actual V1 owners. A private real-SQLite companion now adds
retained WAL/VFS transactions, service-local frame commitment before publication,
distinct client success and durable receiver/copy/restart/promotion journeys.
Public facade selection, production service migration and production
host/reporting integration remain pending. Named progress/report/schema/
controller/election/rejoin preparation and authenticated listener/registry
integration remain concrete next adapter boundaries. P4 prepares service/host
adapters. Only P5 atomically switches factory, APIs/services/wire/reports/
controller and removes the old engine. See the
[roadmap](sf-v1-semantic-alignment-plan.md); P1 creates no permanent alternate
engine, endpoint or selector.

[native]: https://github.com/microsoft/service-fabric/tree/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d
[state]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Replication/ReplicatorState.cpp#L39-L126
[qi]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Replication/ComReplicator.h#L12-L35
[manager]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Replication/ReplicaManager.cpp
[qm]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Replication/ReplicationQueueManager.cpp
[queue]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Replication/OperationQueue.cpp
[qh]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Replication/OperationQueue.h#L62-L87
[secondary]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Replication/SecondaryReplicator.cpp
[receiver]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Replication/SecondaryReplicationReceiver.cpp
[copy-receiver]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Replication/SecondaryCopyReceiver.cpp#L24-L80
[callback]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Replication/ComFromBytesOperation.cpp#L167-L224
[idl]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/idl/public/FabricRuntime.idl#L636-L683
[sender]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Replication/ReliableOperationSender.cpp
[remote]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Replication/RemoteSession.cpp
[ack]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Replication/AckSender.cpp
[copy]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Replication/CopyAsyncOperation.cpp
[build]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Replication/PrimaryReplicator.BuildIdleAsyncOperation.cpp#L111-L166
[copy-sender]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Replication/CopySender.cpp#L524-L579
[transport]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Replication/ReplicationTransport.cpp#L880-L937
[primary]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Replication/PrimaryReplicator.cpp#L632-L683
[header]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Replication/ReplicationOperationHeader.h#L25-L42
[epoch]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Replication/PrimaryReplicator.UpdateEpochAsyncOperation.cpp#L31-L90
[loss]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Replication/Replicator.OnDatalossAsyncOperation.cpp#L82-L126
[status]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Failover/ra/FailoverUnitProxy.ReadWriteStatusCalculator.cpp#L34-L145
[com]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Replication/ComReplicator.cpp
[defaults]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/ServiceModel/reliability/replication/REConfigMacros.h
[partition]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Failover/ra/ComStatefulServicePartition.cpp#L208-L350
[drain]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Replication/SecondaryReplicator.DrainQueueAsyncOperation.cpp#L56-L102
[health]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Replication/BatchedHealthReporter.cpp#L62-L125
[api-test]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Replication/PublicApi.Test.cpp#L454-L504
[queue-test]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Replication/OperationQueue.Test.cpp
[manager-test]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Replication/ReplicaManager.Test.cpp#L154-L220
[catchup]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Replication/PrimaryReplicator.CatchupAsyncOperation.cpp#L43-L104
[sender-test]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Replication/ReliableOperationSender.Test.cpp#L102-L143
[stream]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Replication/OperationStream.cpp#L66-L85
[enumerator]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Replication/ComProxyAsyncEnumOperation.cpp
[copy-test]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Replication/CopyAsyncOperation.Test.cpp#L418-L449
[reconfig]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Replication/reconfig.h#L12-L17
[transport-test]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Replication/ReplicationTransport.Test.cpp#L594-L617
[session]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Replication/ReplicationSession.cpp
[ack-body]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Replication/ReplicationAckMessageBody.h#L17-L24
[close]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Replication/Replicator.CloseAsyncOperation.cpp#L134-L192
[outer-close]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Failover/ra/FailoverUnitProxy.CloseAsyncOperation.cpp#L209-L276
[health-test]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Replication/BatchedHealthReporter.Test.cpp#L107-L135
[types]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/idl/public/FabricTypes.idl#L2910-L2960
[stream-qi]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Replication/ComOperationStream.h#L12-L24
[service-group]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/ServiceGroup/Service/ServiceGroup.StatefulService.h#L153-L180
[com-stream]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Replication/ComOperationStream.cpp#L48-L78
[provider]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Replication/ComProxyStateProvider.cpp#L19-L26
[v2]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/data/txnreplicator/ComStateProvider.h#L10-L29
[get-operation]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Replication/OperationStream.GetOperationAsyncOperation.cpp#L116-L136
[settings-test]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Replication/ReplicatorSettings.Test.cpp#L706-L795
[read-fencing]: https://github.com/youyuanwu/kuberic/blob/aa6c84224437d7e1ba6c5ad3d227f9426026f616/kuberic-runtime/tests/support/read_fencing.rs
[recovery]: https://github.com/youyuanwu/kuberic/blob/aa6c84224437d7e1ba6c5ad3d227f9426026f616/kuberic-runtime/tests/public_v1_recovery.rs#L3523-L3608
[current]: https://github.com/youyuanwu/kuberic/blob/aa6c84224437d7e1ba6c5ad3d227f9426026f616/kuberic-runtime/src/replicator/stateless/mod.rs
[current-stream]: https://github.com/youyuanwu/kuberic/blob/aa6c84224437d7e1ba6c5ad3d227f9426026f616/kuberic-runtime/src/replicator/stream.rs#L315-L365
[current-host]: https://github.com/youyuanwu/kuberic/blob/aa6c84224437d7e1ba6c5ad3d227f9426026f616/kuberic-runtime/src/host/hosting.rs#L545-L605
