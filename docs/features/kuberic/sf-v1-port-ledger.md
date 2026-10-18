# SF V1 Port Foundation Ledger

**P1 foundation, unselected P2 substrate slices and bounded P3 primary and
secondary ownership slices; no selected production behavior change.** Native pin:
[`3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d`][native].
This ledger implements the foundation deliverables in the
[faithful-port roadmap](sf-v1-semantic-alignment-plan.md#dependency-ordered-implementation-program).
The current bespoke engine is the migration/safety baseline, not the native
specification. `v1/` destinations below are under
`kuberic-runtime/src/replicator/`; OperationQueue and the bounded components
described below are implemented, and none is activated. P1 itself implemented
no component.

## Evidence and status

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

## Source families, transitions and substitutions

| ID / native family and source | Transitions/responsibility | Rust destination (only explicit slices implemented) | Allowed representation substitution | Scenarios |
|---|---|---|---|---|
| F01 facade/factory/ReplicatorState ([state:39–126][state], [QI:12–35][qi]) | initial/transitional/primary/secondary/faulted/closed; role admission | `v1/replicator.rs`, `state.rs`, existing facade | COM/refcounts → owned Rust, not altered role outcomes | A1, C1, L1 |
| F02 PrimaryReplicator/ReplicaManager/ReplicationQueueManager ([manager:349–421][manager], [queue-manager:261–408][qm]) | CC/PC/idle sessions; enqueue/assignment/completion; quorum/config capture | bounded `v1/replication_queue_manager.rs`, `replica_manager.rs`; full `primary.rs` remains pending | owned futures replace Begin/End, not primary ACK, configured W or B/Q/M arithmetic | A1, M1–M5, E1 |
| F03 OperationQueue ([queue:677–737,990–1047][queue], [heads:62–87][qh]) | gap/order, retention, count/byte admission, commit/complete/trim/reset/transfer | `v1/operation_queue.rs` | owned storage instead of ring layout, not a plain channel or merged cursors | Q1–Q4, L2 |
| F04 SecondaryReplicator/receivers ([secondary:165–193,951–1003][secondary], [receiver:632–740][receiver]) | reorder/dispatch/completion, fixed copy/minimum replication epoch, drains | bounded `v1/secondary_receiver.rs`; full secondary/role owner pending | async queues/executor; required/optimistic ACK and explicit epoch handoffs delivered, copy/provider/drains pending | S1–S3, E2 |
| F05 operation/data/stream/provider ([stream:24–80][copy-receiver], [callback:167–224][callback], [IDL:636–683][idl]) | shared getters, data/EOF/EOS, late callback states, provider enumeration | existing protected facade + `v1/buffers.rs`, `received_operation.rs`, `operation_stream.rs` | protected Bytes/boxed stream/i64 unchanged; codec and bounded replication callback/stream owners delivered, copy/provider/public facade integration pending | D1, S2–S3, C1–C2 |
| F06 ReplicationSession/RemoteSession/ReliableOperationSender/AckSender ([sender:548–755][sender], [ACK order:585–627][remote], [batch:64–112,174–208][ack]) | incarnation, separate receive/apply, retry/window/RequestAck, final ACK | `v1/session.rs`, `sender.rs`, `ack.rs` | tasks/injected clocks, not deleting native retry/flow control because HTTP/2 is reliable | R1–R3, T2 |
| F07 CopySender/CopyAsyncOperation/context/establishment ([copy:171–254][copy], [build:111–166][build], [retarget:524–579][copy-sender]) | target context, serial polls, one parked item, EOF target and pending retarget | `v1/copy.rs`, provider/stream adapters | bounded async queues/futures, not source-local context or a fixed snapshot-only gate | C2–C5 |
| F08 transport/demux/headers ([send:880–937][transport], [session lookup:632–683][primary], [header:25–42][header]) | logical messages, full identity, two epochs, sequence spaces, expiry | `v1/transport.rs`, private `sf_v1.proto` | tonic carrier, no native wire compatibility; accepted send is not service ACK; host/role demux integration pending | T1–T2, D1 |
| F09 provider epoch/recovery/RA boundary ([epoch:31–90][epoch], [reset:82–126][loss], [status:34–145][status]) | issued epoch boundary, queue recovery/transfer, lifecycle access/direct vs outer Close | `v1` + existing host/controller adapters | no RA/FM/placement port; optional provider durability is not native queue progress | E1–E2, L2–L3, H1–H2 |
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
bounded primary queue/quorum core; full primary, receivers/copy/roles remain.
P4 prepares service/host
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
