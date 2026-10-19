# SF V1 Port Provenance and Notices

## Selected upstream and license

The P1 [port ledger](sf-v1-port-ledger.md), including its documented acceptance
scenarios and selected native test expectations, derives from Microsoft Service
Fabric at immutable commit
[`3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d`](https://github.com/microsoft/service-fabric/tree/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d).
The authorized reference checkout was reviewed read-only and clean at that pin.
No native binary or native test was run. P1 incorporated no implementation;
the queue, bounded sender/session/ACK, async/tonic, primary and secondary ownership slices below translate selected native production
algorithms, message contracts and test assertions into Rust. No native transport library or test
harness is incorporated.

Copyright (c) Microsoft Corporation. All rights reserved.

The native root file is
[`LICENSE`](https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/LICENSE),
**not `License.txt`**, despite the latter name in some upstream source comments.
Its complete Microsoft MIT permission, inclusion requirement and disclaimer are
retained verbatim in [`LICENSES/service-fabric-MIT.txt`](../../../LICENSES/service-fabric-MIT.txt).
See also [`THIRD-PARTY-NOTICES`](../../../THIRD-PARTY-NOTICES).
Kuberic's root license and existing API-documentation notices are unchanged.

## Concrete material register

### OperationQueue implementation slice (P2, not all of P2)

Rust destinations:
[`operation_queue.rs`](../../../kuberic-runtime/src/replicator/v1/operation_queue.rs)
and its
[`component tests`](../../../kuberic-runtime/src/replicator/v1/operation_queue/tests.rs).
Both retain the selected Microsoft copyright/MIT header; the complete
permission/disclaimer is retained in the license file linked above.

| Selected native material at the pin above (`R/` prefix below) | Translation / representation changes |
|---|---|
| `OperationQueue.h:21–188`; `OperationQueue.cpp:26–180,194–203,214–236` | Internal synchronous owned queue, configuration, four cursors, transfer and callback installation. `Arc`/`Bytes` replace COM references/data accessor; a dynamic atomic needs-ACK predicate is a dependency, not a receiver ACK implementation. No public alias changes. |
| `OperationQueue.cpp:256–668,670–987,990–1306,1347–1389` | Actual admission, span/count/byte limits, native resize/convergence, contiguous commit/complete, retention/trim, rewinds/discards/reset. Ordered owned map replaces ring allocation while preserving positional capacity policy. Out-of-retained-range ring aliasing is not an API capability. |
| `OperationQueue.Test.cpp:403–426,435–605,607–615,867–901,995–1095` | Selected growth/history/memory/gap/commit/trim assertions adapted into descriptive real-component tests; randomized fixtures reduced to deterministic arrivals. The earlier-hole case uses the actual 598-byte call at 490–493, not the stale neighboring comment. Q1–Q4/L2 IDs are ancestry only, not a second test catalog. |
| Production `OperationQueue.cpp:93–180,214–236,677–987,1128–1306` | Additional tests of service-ACK eligibility, callback call-point progress/self-disable/replay, targeted prerequisites, transfer non-normalization, reset and externally retained lifetime. These derive from production; native queue tests do not directly exercise ACK-required/callback mode. |

The callback takes an immutable progress snapshot at the native call point
and returns keep/disable instead of permitting arbitrary reentrant mutation.
Ordinary commit calls back before advancing commit; ignore-commit completion
advances commit first and completion afterward. Lifecycle timestamp refresh,
trace/performance sinks and description/partition diagnostics are omitted,
not replaced with provider/ACK effects. Resize averaging remains algorithmic;
its power-of-two result is computed on demand rather than cached. Valid
representable LSN/capacity intervals and allocatable storage are prerequisites;
no allocator/extreme-number recovery contract is claimed.

Selected files' Microsoft MIT headers and root `LICENSE` were reviewed.
No Boost/Common/Transport/KTL utility implementation, fixture, third-party
asset or dependency graph was translated. The heterogeneous umbrella notices
therefore do not become a blanket license for this selection. No new
third-party dependency is introduced. Queue components remain internal and
unselected; component execution is not native execution, differential parity,
full P2, or P5 acceptance.

### Sender Session ACK Implementation Slice (P2, Bounded F06)

Rust destinations (each production/test file retains Microsoft copyright/MIT):
[`sender.rs`](../../../kuberic-runtime/src/replicator/v1/sender.rs),
[`sender tests`](../../../kuberic-runtime/src/replicator/v1/sender/tests.rs),
[`ack.rs`](../../../kuberic-runtime/src/replicator/v1/ack.rs),
[`ACK tests`](../../../kuberic-runtime/src/replicator/v1/ack/tests.rs),
[`session.rs`](../../../kuberic-runtime/src/replicator/v1/session.rs) and
[`session tests`](../../../kuberic-runtime/src/replicator/v1/session/tests.rs).
`R/` below is `src/prod/src/Reliability/Replication/` at the same immutable pin.

| Selected native production/test material | Translation / representation changes |
|---|---|
| `R/ReliableOperationSender.h:158–178,209–235`; `.cpp:25–205,207–472,509–823` | Synchronous sender state, single/replay merge, receive/apply progress, hint, count-window adaptation, retry selection/rearm and timer-only RequestAck. Existing `Arc<QueueOperation>` replaces owner-backed raw entries; selected batches strongly own operations before outside-guard dispatch. No second OperationQueue, byte window or LSN allocator. Relative injected time uses explicit never-selected state. |
| `R/ReliableOperationSender.cpp:899–985`; `R/REPerformanceCounters.h:12–20`; `R/DecayAverage.cpp:19–119` | Admission receive/apply watches, positional ACK traversal, sequential signed integer tick means, strict collection threshold, dynamic decay, unsigned-millisecond saturation/reset. Safe iteration does not copy native out-of-range iterator UB; arbitrary sparse latency domains are not certified. Trace/performance sinks and dynamic config-store integration are not ported. |
| `R/AckSender.h:15–62`; `.cpp:24–225` | Callback batching, force/pending-only final flush, construction-time force gate, last-issuance and trace decisions. Owned dispatch captures callback/root, not payload. Timer entry remains distinct from disabling the deadline: already-dispatched active callbacks may still run; close is not a join. Native running trace stopwatch persists on reopen. |
| Selected `R/RemoteSession.cpp:33–169,434–446,448–655,757–790`; `R/ReplicationSession.cpp:278–325,426–463` and declarations in their headers | Partial session identity/current epoch/progress/idle view, ACK coalescing and ordered processor, replication/RequestAck intents and retry/infinite expiration policy. Short mutex transitions and move-only rooted handoffs replace native posted/rooted ACK work. Apply is sampled after sender dispatch; processor finish/recheck is atomic with ingress. Current epoch is sampled separately at each send. No invented RequestAck epoch/hint payload. The selected `:757–790` expiry/queue-full continuation contract does not implement its deferred carrier status-to-fault mapping. |
| Selected `R/RemoteSession.cpp:99–112,243–259,591–595` | Close/open and optional establishment-owned StartCopy sender close seam only. Does not implement establishment, context receiver, copy cancellation/provider polling, full StartCopy delivery or role drainage. Selected work survives close; post-close ACK processing is not rejected. |
| `R/ReliableOperationSender.Test.cpp:102–143` **TestReceiveACKProgress**; `:145–199` **TestAddRemove**; `:201–236` **TestAddMoreThanContinuousSend**; `:238–310` three **TestMaxSendingWindowSize** cases | Selected receive/apply/pending/timer/window assertions adapted to actual Rust sender and shared queue entries. Named tests preserve ancestry, not a wholesale fixture translation. Manual time replaces sleeps; deterministic arrivals replace shuffles/racing timer tolerance. |
| `R/ReliableOperationSender.Test.cpp:312–445` **TestOperationLatencyList** and helpers; `R/DecayAverage.Test.cpp:28–63` **NoDecay_Basic**, **NoDecay_Overflow**, **SimpleAverage**, **SimpleAverageOverflow** | Separate stopwatch and collection assertions, latest-value/running-average/saturation expectations adapted to actual sender/helper calls. Dense fixed small cases replace random 100-item fixtures; no native test execution or all-domain latency equivalence claimed. |
| Production ranges above, especially sender `:339–472,548–787`, ACK `:99–225`, session `:492–655` | Additional actual-component tests for replay identity/time/hints, exact retries, backpressure, dynamic collection/decay, force/trace/final-flush races, ownership/close/reopen, pair replacement/copy terminal/flags, reentrant epochs, ordered handoffs and concurrent ingress. These are production-derived, **not** mislabeled dedicated native AckSender/RemoteSession unit tests. |

**Measured component scope:** 28 sender, 13 ACK and 20 session tests; 61 new
tests, 103 combined with the existing 42 queue tests. All use actual compiled
components; session tests deliver explicit copy/primary obligations without
pretending to execute their missing owners. These sources were independently
reviewed against native and tested locally; native binaries/tests were read,
not built/executed.

**Reviewed context, not translated owner implementations:** actual caller
relationships in `ReplicaManager.cpp:107–147,1138–1242,1333–1345,1835–1925`,
`PrimaryReplicator.cpp:632–750`, `CopySender.cpp:56–85,267–406`,
`CopyAsyncOperation.cpp:42–125,237–335,367–378`,
`CopyContextReceiver.cpp:80–127,154–237`,
`SecondaryReplicator.cpp:681–824,1214–1261,2099–2107`,
receiver progress/threshold code, and build/establishment paths. Native
`ReplicateCopyOperations.Test.cpp:744–878` pending-count/RequestAck integration
and `CopyAsyncOperation.Test.cpp:276–303,352–449` advisory cancellation were
read for boundaries, not ported/executed. `Common/Timer.h:33–56`,
`Timer.Linux.cpp:452–489`, `Stopwatch.cpp:49–115`, `TimeSpan.h` and
`AsyncOperation.cpp:150–195` were consulted for timer/time/cancellation semantics;
no platform utility implementation is copied.

**Not implemented by this sender/session slice:** carrier/runtime/buffer
integration (recorded separately below), manager/receiver/copy/provider/role owners; ReplicationSession
slow-fault flags/notification timer, MustCatchup and promotion-owner mechanics.
An epoch/progress view or handoff is not build/quorum/copy completion. Existing
protected aliases/interfaces and selected production factory remain unchanged.
No new dependency, native harness, Common/Transport/KTL/Boost implementation or
dependency graph is imported. Full P2 and activation/hosted/native/differential
parity remain unestablished.

### Async Tonic Substrate Slice (P2, Bounded F05/F08/F11)

Rust destinations are private
[`transport.rs`](../../../kuberic-runtime/src/replicator/v1/transport.rs),
[`buffers.rs`](../../../kuberic-runtime/src/replicator/v1/buffers.rs),
[`async_runtime.rs`](../../../kuberic-runtime/src/replicator/v1/async_runtime.rs)
and their component tests. The new private
[`sf_v1.proto`](../../../kuberic-runtime/proto/sf_v1.proto) is a Rust/tonic
logical-message encoding, not the native wire format or an activated listener.

| Selected native material at the same pin (`R/` as above) | Translation / representation changes |
|---|---|
| `R/ReplicationEndpointId.h:23–80`; `ReplicationTransport.cpp:919–945` | Full partition/replica/incarnation routing and sender-address fields. Bound route and superseded-stream fencing replace native actor transport plumbing; authorization and role admission remain future owners. |
| `R/ReplicationTransport.cpp:168–266,268–355,684–878`; `ReplicationOperationHeader.h:25–110`; `StartCopyMessageBody.h:17–47` | Independent ACK/error fields, distinct sending/original epochs, metadata/segments/batch boundaries, copy/context terminal markers, StartCopy, payload-free RequestAck and targeted InduceFault. A typed bidirectional tonic exchange replaces native one-way transport; its acceptance never supplies application ACK. |
| `R/ReplicationTransport.cpp:880–917`; `RemoteSession.cpp:757–790` | Per-message retry/infinite expiry, queue-full-only batch stop, explicit too-large/failure obligations. Count/byte-bounded Rust queues and typed outcomes are the carrier adaptation, not native transport implementation. |
| `R/ReliableOperationSender.cpp:113–137,548–755`; `R/AckSender.cpp:64–208`; `R/RemoteSession.cpp:448–655` | Tokio clock/timer ownership drives existing translated components, preserving outside-lock selected work, batching/final ACK and ordered processor handoffs. No Common/Timer implementation or platform cancellation code is translated. |
| Public `FabricRuntime.idl:626–683`; `src/prod/src/managed/Api/src/System/Fabric/OperationData.cs:16–38`; roadmap section 6 | Ordered buffer-array semantics inform explicit raw-single/framed-segments conversion through unchanged protected byte streams. The versioned frame and checked decoder are Kuberic-authored representation adaptations, not native serialization. |
| `R/ComOperation.cpp:100–115`; existing `OperationQueue.cpp` byte admission/history/removal paths | Explicit queue-data construction validates the chosen representation and stores logical payload size, excluding Rust frame overhead. Raw construction is unchanged. Actual queue regressions cover exact byte capacity, suffix displacement, completion/rewind/trim/discard and cleanup with framed entries. |
| `R/ReplicationTransport.Test.cpp:594–617` **TestDropMessageToNotMatch**, plus production ranges above | Actual Rust bound-route/stale-incarnation and carrier tests use local tonic and deterministic inputs, not the native sockets/sleep fixture. Other carrier/runtime/frame cases derive from production obligations and the approved Rust representation, not translated native test executions. |

The source-guided transport/runtime files retain Microsoft copyright/MIT
attribution; the complete permission/disclaimer remains in
`LICENSES/service-fabric-MIT.txt`. Generated
protobuf code and Tokio/tonic execution replace platform plumbing; no native
Common/Transport/KTL/Boost utility, native wire serializer or test harness is
imported. No new production dependency is needed; Tokio's existing test-util
feature is enabled for deterministic timer tests.

Runtime effect staging has a separate fixed 64-entry adapter limit and reserves
ACK final-close capacity. It is not a native REConfig default. Capacity errors
precede core mutation; shutdown closes runtime ingress while native close alone
does not reject rooted ACK work. Explicit runtime shutdown joins selected work;
unjoined Drop is typed abnormal abort containment, not native graceful drain.
The carrier itself spawns no tasks: its client driver and server future must
remain explicitly owned by their caller.

There are **54 new actual tests**: 27 runtime, 20 carrier, four buffer and three
queue-accounting regressions. Combined v1 coverage is **157**. Review regressions
preserve per-generation readiness wakers (late old polls cannot strand a new
stream), exact failed-Exchange RPC status, shutdown-time ingress rejection and
bounded effect admission. Real tonic tests also connect the session runtime and
reverse ACK runtime end to end; recording owner handoffs are not copy/build/
quorum implementations.

This remains preparatory P2 work. No primary/secondary/copy/role machine,
service durability, native slow-peer policy, MustCatchup/promotion, host
credentials/controller integration, production activation, complete P2,
native/differential execution or hosted parity is established.

### Primary Queue and Quorum Slice (P3, Bounded F02/F06/F09)

Rust destinations are private
[`replication_queue_manager.rs`](../../../kuberic-runtime/src/replicator/v1/replication_queue_manager.rs),
[`replica_manager.rs`](../../../kuberic-runtime/src/replicator/v1/replica_manager.rs)
and their component tests. Selected production/test files retain the Microsoft
copyright/MIT attribution and the existing complete notice above.

| Selected native material at the same pin (`R/` as above) | Translation / representation changes |
|---|---|
| `R/ReplicationQueueManager.cpp:29–91,99–160,209–236,261–408`; declarations in its header | Primary queue construction/normalization, issued/committed/capability views, configuration capture, commit/completion projection and admission-only LSN advancement. Existing `Arc<QueueOperation>`/`SenderOperation` retain operation data and original epochs; explicit buffer policy preserves logical-byte accounting. None represents native NonInitializedLSN, not missing-input success. |
| `R/ReplicaManager.cpp:349–447,1061–1103,1244–1374,2328–2525` | Configured current/previous quorum, receive-based reclamation, all-current apply view, quorum-one asymmetry, catch-up-config cap and MustCatchup. Concrete established Rust sessions replace native descriptor-driven session creation; full native configuration lifecycle, slow-fault exclusion and promotion remain explicit owners. |
| `R/ReplicaManager.cpp:107–147,792–869,2076–2197,2266–2275,2351–2411` | Admission and separately owned completion, multicast/replay effects and terminal outcome staging replace COM AsyncOperation/job queues. Caller serialization and outside-owner delivery replace native locks/job queues; service-local commit is not a new primary prerequisite. |
| `R/PrimaryReplicator.CatchupAsyncOperation.cpp:43–104`; `R/PrimaryReplicator.cpp:383–419,761–832` | One pending catch-up owner, moving All versus saved WriteQuorum boundary, current-quorum-relative marked minimum, insufficient-quorum pending and terminal completion. Owned Rust receivers replace Begin/End; dropping a waiter is not rollback of assigned operations. |
| `R/ReplicaManager.Test.cpp:154–220` **TestUpdateConfigurationWithQuorumIgnorePreviousConfig**, **TestUpdateConfigurationNoQuorumIgnorePreviousConfig**; production ranges above | Selected quorum progress inputs/expectations become tests of actual Rust managers and sessions. No native transport/partition wrapper, sleeps, random fixture, Boost harness or duplicated executable scenario catalog is translated. Other cases derive from production, not native test execution. |

The slice contains **44 actual tests**: 13 queue owner and 31 replica manager,
taking combined v1 coverage to **201**. Native binaries/tests were not executed.
Single-model review corrections preserve cached current-configuration progress
until its ordered primary handoff, concrete session-root reuse, atomic
configuration/removal failures and observable shutdown-update failure after
cleanup. A further admission regression rejects ahead-of-tail ACKs even when
quorum membership is insufficient. These validation guards are Rust boundary
adaptations, not native transport/role admission. Idle progress is shared by
session and manager through one checked sender-progress calculation.

The [ledger's remaining-owner table](sf-v1-port-ledger.md#primary-queue-and-quorum-slice-p3-bounded-f02f06f09)
maps slow-fault eligibility, pressure-driven idle retarget, session fault flags/
sender shutdown/InduceFault retry and full promotion/role/copy/provider duties.
Reading these source paths does not claim their algorithms are implemented.
Queue-transfer normalization, where exercised independently, is not a
secondary-to-primary or demotion lifecycle.

No new dependency, protected trait/type, public API, selected engine/factory/
listener/wire or host/controller change is introduced. No native implementation
binary, Common/Transport/KTL utility or test harness is imported. Component
execution is not full P2/P3, native/differential, hosted or provider-fsync parity.

### Secondary Receiver and ACK Slice (P3, Bounded F04/F05/F06/F09)

Rust destinations, each retaining Microsoft copyright/MIT attribution:
[`received_operation.rs`](../../../kuberic-runtime/src/replicator/v1/received_operation.rs),
[`operation_stream.rs`](../../../kuberic-runtime/src/replicator/v1/operation_stream.rs),
[`secondary_receiver.rs`](../../../kuberic-runtime/src/replicator/v1/secondary_receiver.rs)
and their adjacent `tests.rs` component suites. `R/` remains
`src/prod/src/Reliability/Replication/` at the immutable pin above.

| Selected native material | Translation / representation changes |
|---|---|
| `R/ComFromBytesOperation.h`; `.cpp:127–257`; selected `ComOperation.h` metadata/NeedsAck seam | Actual five-state service callback lifecycle, ordinary cancellation, EOS/root retention and repeated/late ACK behavior. Rust `Arc`/mutex and move-only `AckWork` replace COM callback invocation/refcounts. ReadyToRun blocks the existing queue; Running releases its atomic predicate. Callback completion/owner release runs outside the receiver guard. The protected operation-data/stream aliases remain unchanged. |
| `R/SecondaryReplicationReceiver.h`; `.cpp:157–224,250–343,345–444,632–749` | Existing actual OperationQueue owns received/complete/history cursors, gaps, duplicates, span/byte pressure and primary completed hints. Caller-supplied StartCopy initialization does not execute copy. Metadata maps retain original epochs without replacing a duplicate root. Received and service completion remain distinct; volatile default completes on dispatch admission. Typed validation errors are Rust input-boundary guards. |
| `R/SecondaryReplicationReceiver.cpp:446–630`; `R/ComUpdateEpochOperation.h` | Initial original-epoch establishment, internal stream marker versus full required-ACK barrier, captured previous-epoch last LSN and callback disable/replay. Explicit matching success resumes work; abandoned handoffs do not. Provider invocation and full native fault/drain aftermath remain absent. Error completion performs explicit receiver failure containment, not successful native Close. |
| `R/OperationStream.h/.cpp`; `R/OperationStream.GetOperationAsyncOperation.h/.cpp`; `R/ReplicationPointers.h:33` | Shared consuming dispatch state, pending/open versus EOF/EOS and internal epoch interception. Rust futures/waker registration replaces Begin/End/ReaderQueue plumbing; no Common utility source is copied. A shared barrier prevents another handle bypassing an outstanding provider request. Sticky abort error once per handle, staged aborted-entry release and broadcast/cancellation-safe wakeups are explicit Rust adapter policies, not native COM error-result parity. |
| `R/SecondaryReplicator.cpp:165–193,324–350,675–679,1306–1396,1624–1701,2087–2108`; `R/Constants.cpp:15–19` | Persisted-or-required mode, no callback for optimistic ACK, moving sending-epoch admission, service-ACK progress, native pending-count threshold and outside-guard scheduling. Existing AckSender samples the current receiver through its actual callback; the real copy owner must supply its pending count. Native InvalidLSN=0 and NonInitializedLSN=-1 stay distinct. Full role/demux/authentication/provider orchestration is not imported. |
| `R/ReplicationSecondary.Test.cpp:1951–2010` **OutOfOrderReplication**, **OutOfOrderReplicationNoReply**, **OutOfOrderReplicationExceedingQueueCapacity**, **DuplicateReplication**; `:2052–2105` **MultipleOperationEpochs**, **VerifyStateProviderEstablishedEpochTest** | Selected ordering, duplicate/capacity and original-epoch expectations adapted to actual Rust components. Deterministic inputs/manual ACK delivery replace native wrappers, random ACK shuffles, sleeps and Boost fixtures. No native test or binary execution. Remaining callback/concurrency/wake/terminal tests derive from production branches and Rust ownership obligations. |

**Measured scope:** 31 received-operation + 22 operation-stream + 39 receiver/
integration tests = **92 new tests**, **293 combined v1**. The integrated runtime
library passed **350/350**, including 57 other library tests. Tests include actual
queue scans, concurrent ACK/retirement, owner-drop/reentry, multiple/cancelled
readers, explicit successful/failed/abandoned barriers, real ACK timing/snapshot
and a primary-manager-to-receiver/session trace deliberately stopped before its
unimplemented copy handoff. No test-local replication formula or executable
ledger copy is introduced.

Single-model integration review corrected callback identity after queue cleanup:
already-selected original callbacks remain addressable after reset, including
terminal retirement, while foreign same-LSN or other-receiver callbacks cannot
release a live receiver's queue predicate. All effect/callback wakeups remain
outside serialization. Failed provider completion contains this component;
native `SecondaryReplicator.cpp:1740–1860,2247–2286` fault reporting, two-receiver
EOS/cleanup and drain completion remain future owner integration, not a claim
of exact native fault lifecycle.

No native Common/Transport/KTL/Boost code, native test fixture, new dependency,
protected interface, selected listener/factory/wire, service persistence or host/
controller code is imported or changed. See the [remaining-owner ledger](sf-v1-port-ledger.md#secondary-receiver-and-ack-slice-p3-bounded-f04f05f06f09).
This is bounded preparatory ownership, not full F04/F05, P2/P3 completion,
native/differential/hosted parity or production activation.

### Copy Receivers Slice (P3, Bounded F04/F05/F07)

Rust destinations, each retaining Microsoft copyright/MIT attribution:
[`secondary_copy_receiver.rs`](../../../kuberic-runtime/src/replicator/v1/secondary_copy_receiver.rs),
[`copy_context_receiver.rs`](../../../kuberic-runtime/src/replicator/v1/copy_context_receiver.rs)
and their adjacent component `tests.rs` suites;
[`copy_receiver_integration.rs`](../../../kuberic-runtime/src/replicator/v1/copy_receiver_integration.rs)
adds actual cross-component boundary witnesses. The existing received-operation
implementation/tests add a checked numbered network-EOS constructor.
`R/` remains `src/prod/src/Reliability/Replication/` at the immutable pin above.

| Selected native material | Translation / representation changes |
|---|---|
| `R/SecondaryCopyReceiver.h:40–54,72`; `.cpp:24–184,186–285,302–455` | Actual count-only ordering/dispatch, required/optimistic ACK progress, last-marker/gap/full/duplicate behavior, copyDone, final sentinels, queue close, local cleanup differences and EOS/root lifetime. Existing Rust queue/callback/shared-stream primitives replace native pointers/ReaderQueue. ACK/copy-sender checks are explicit obligations, not executed sends or build completion. The legacy terminal/wait predicate is preserved rather than guessed away. |
| `R/CopyContextReceiver.h:12–85`; `.cpp:27–63,80–97,105–145,147–234` | Primary-side target-context receipt, no-service-ACK queue, numbered terminal bookkeeping, strict pending threshold, current issuance snapshot and close/error/final ACK ordering. Caller-serialized transitions replace the receiver lock. Small snapshot/close Arcs let selected ACKs and data-only streams outlive the receiver without copying native root cycles. |
| `R/ComOperationDataAsyncEnumerator.cpp:59–75,90–157,188–196` | Data-only shared consuming stream; successful explicit close permits an already-pending dequeue, subsequent calls fail, error close fails pending reads. Rust polling replaces Begin/End; cancellation does not undo the handle's pending registration. Shared close state across handles and one error followed by terminal EOF are explicit Rust policies. Error close immediately retires inaccessible data rather than retaining COM wrappers until failed dequeue/destruction. |
| `R/ReplicationTransport.cpp:722–752,776–802`; `R/ComFromBytesOperation.cpp:11–24`; `R/Constants.cpp:15–19` | Native last-message conversion preserves positive protocol LSN, marks copy/context EOS independent of policy and uses invalid operation epoch `(0,0)`. A small shared checked constructor preserves those distinctions from zero-LSN local EOS. Terminal classification uses logical payload length, not framed buffer-adapter overhead. No transport implementation is imported here. |
| `R/AckSender.cpp:98–208`; `R/ReplicationSession.cpp:466–509`; `R/SecondaryReplicator.cpp:681–825,1500–1570,1967–2055,2087–2108`; `R/RemoteSession.cpp:585–627` | Existing real scheduler/current-state ACK callback, combined copy+replication pending threshold, ordered session handoff and explicit copy-context-sender/drain obligations. Rust effects stage callbacks, wakers, ACK finishes and retired/rejected input destruction outside caller guards. No copy sender, provider/drain or host-report success is fabricated. |
| `R/ReplicationSecondary.Test.cpp:1726–1911`; `R/ReplicateCopyOperations.Test.cpp:430–438,744–807,1049–1108,1972–2013` | Selected out-of-order/duplicate/capacity/last/zero-context/batching/EOS expectations adapted to actual receivers and existing primitives. Deterministic input and injected time replace wrappers, network fixtures, random ACKs, sleeps and Boost harnesses. Other identity, frame, reentrant-drop/wake, terminal and close regressions derive from production branches and Rust ownership obligations. No native tests execute. |

Integration review corrected canonical network-EOS admission in legacy copy
mode and in context terminal handling, and made framed terminal validation use
logical bytes. Numbered terminal metadata cannot become service data or a
replication-LSN claim. Selected/late callbacks retain exact receiver identity
after cleanup. Replaced EOS handles and rejected input/work roots retire only
through outside-guard effects.

**Measured scope:** 31 secondary-copy + 33 copy-context + two numbered-EOS
constructor + two actual pair/session witnesses = **68 new tests**, **361
combined v1**. The integrated runtime library passed **418/418**, including
57 other library cases. Exact workspace discovery is **776 = 730 ordinary +
41 PostgreSQL + 3 KinD + 2 build-only**; discovery does not execute provider/
cluster/native bodies.

The [bounded ledger](sf-v1-port-ledger.md#copy-receivers-slice-p3-bounded-f04f05f07)
names remaining provider/context acquisition, copy enumeration/sending/build,
establishment, cross-receiver fault/report/drain and role owners. No new
dependency, native Common/Transport/KTL/Boost utility or test harness,
protected type, selected production factory/listener/wire or service/host/
controller change is introduced. This is not full F04/F05/F07 or P2/P3,
native/differential/hosted/provider-fsync parity or production activation.

### Copy Enumeration Slice (P3, Bounded F05/F07/F11)

Rust destinations retain Microsoft copyright/MIT attribution:
[`copy_async_operation.rs`](../../../kuberic-runtime/src/replicator/v1/copy_async_operation.rs)
and adjacent
[`tests.rs`](../../../kuberic-runtime/src/replicator/v1/copy_async_operation/tests.rs).
Existing sender code gains only a read-only pending-operation iterator for
outside-guard lifetime staging; its retry/window/ACK algorithms are unchanged.
`R/` below is `src/prod/src/Reliability/Replication/` at the immutable pin.

| Selected native source | Incorporated responsibility and explicit Rust substitution |
|---|---|
| `R/CopyAsyncOperation.h:12–103`; `.cpp:42–60,70–212` | Count-only clean-on-complete/ignore-commit queue starting at one, serial enumeration, cancellation at full/EOF versus in-flight polling, provider-error precedence and EndGetNext fault retention. A caller-driven Tokio future replaces native posting/Begin-End, without changing the protected stream type or acquiring/invoking the provider. |
| `R/CopyAsyncOperation.cpp:214–271` | Next sequence assignment, one parked full-queue item, numbered EOF, last callback once even when parked and before sender Add. EOF is metadata, not an empty-byte heuristic. The existing explicit buffer adapter validates representation and accounts logical data; no new copy memory limit or ordering queue. |
| `R/CopyAsyncOperation.cpp:273–365` | Sender receive-ACK processing/dispatch precedes applied queue completion and parked retry. Applied progress wakes the driver, never polls provider inline. Final progress sets ready only; it does not invent CopySender replication/build success. ACKs are already-normalized positive/zero sequences; wire sentinel conversion is not imported. |
| `R/CopyAsyncOperation.cpp:367–417` | Separate idempotent End closes reliable sending, exposes provider fault/error description and keeps sender progress/latency views distinct. Explicit success request respects the first completion and the native ready bit. |
| `R/ComProxyAsyncEnumOperation.cpp:90–121,137–195` | Nonnull empty data is an item, null output is EOF, absent native enumerator is an empty stream. Rust callers supply the unchanged boxed stream (including an explicit empty stream). Weak handles avoid callback cycles; a unique lease returns the stream to the rooted owner on drive-future cancellation or exit. Selected work retains the actual provider root through End/owner release. Stream Drop replaces applicable disposal ownership, not COM optional-interface result parity. |
| `R/CopyAsyncOperation.Test.cpp:276–303,320–449` | Native cancellation timing assertions, twelve-item drained success, fifteen-item/eight-slot/ACK-below-four full retry cancellation, plus smaller parked/EOF vectors. Real Rust component tests replace native service wrappers, callback fixtures, sleeps and Boost harness. Production-derived tests cover malformed/overflow/ACK input, in-flight error precedence, timer/RequestAck behavior, framed data, real-executor wakeups and reentrant payload/provider/waker lifetimes. No native test is executed. |
| `R/SecondaryCopyReceiver.cpp:24–184,302–455`; `R/CopyContextReceiver.cpp:105–234` | Real receiver boundary witnesses preserve required/optimistic service ACK, ordinary empty data, numbered copy terminal and data-only context EOF/terminal ACK LSN. Test-only normalization does not implement a production CopySender or build owner. |

Review strengthened selected-work rooting: the provider lives in the actual
callback root, not only beside it in the driver. A safe unique lease avoids
holding any guard across foreign polling and returns the stream on dropped
drive futures. Retired queue/sender payloads and callback wakeups are staged
outside transition guards, including applied-only progress with received zero.
The unique mutable driver is the serialization contract for stream polling;
dropping it does not claim native cancellation/drain or successful completion.
The constructor combines native sender-open and owner initialization; execution
still requires driving the future. Invalid settings, ACKs, representation and
sequence exhaustion surface explicit Rust errors instead of native assertions.

**Measured scope:** **45 new actual tests**, **406 combined V1**, runtime library
**463/463**, with 57 unchanged non-V1 library cases. Exact workspace discovery:
**821 = 775 ordinary + 41 PostgreSQL + 3 KinD + 2 build-only**. Workspace check/
Clippy, no-default runtime, external API/privacy, doctests and exact tier/
partition validation passed. Discovery does not execute cluster/provider tiers.
See the [bounded ledger](sf-v1-port-ledger.md#copy-enumeration-slice-p3-bounded-f05f07f11)
and [test selector](testing.md#sf-v1-copy-enumeration-slice).

At this slice boundary, CopySender `.cpp:56–85,267–406,524–579` and
`PrimaryReplicator.BuildIdleAsyncOperation.cpp:111–166` remain reviewed context,
not ported build/replication gates or retargeting. Provider/context acquisition,
StartCopy establishment, fault/drain/role orchestration and facade/service/
host/controller cutover remain future owners. Native ApiMonitoringWrapper
slow-provider monitoring/health reporting is not incorporated by this slice.
No dependency, utility library,
native harness, protected API, selected production wiring, native/differential/
hosted/provider-fsync parity or complete P2/P3 claim is introduced.

### CopySender Build Gates Slice (P3, Bounded F06/F07/F11)

Rust destinations retain Microsoft copyright/MIT attribution:
[`copy_sender.rs`](../../../kuberic-runtime/src/replicator/v1/copy_sender.rs)
and adjacent
[`tests.rs`](../../../kuberic-runtime/src/replicator/v1/copy_sender/tests.rs).
The existing enumeration owner gains only an explicit outside-guard-waking
external-error completion entry and private typed errors; its queue, sender,
provider stream and serial enumeration algorithms are unchanged.
`R/` denotes `src/prod/src/Reliability/Replication/` at the same immutable pin.

| Selected native source | Incorporated responsibility and explicit Rust substitution |
|---|---|
| `R/CopySender.h:29–157`; `.cpp:26–265` | One-shot Begin, stale/pre-Begin failure, explicit FinishOperation, EOF callback, separate End and completion-disable race fault. Rust uses a caller-owned driver, short transitions, weak handles and an EOF-callback-rooted sender state instead of COM/Begin-End. A prefailed Begin never polls the provider. First completion wins, including synthetic prefailed completion. |
| `R/CopySender.cpp:267–406` | Replication gate requires terminal-copy evidence before it latches; copy/application completion remains separate. Normalize copy `-1` to the known terminal, preserving zero/no-update and receive-versus-applied progress. Complete/update enumeration outside the sender guard. Disabling completion suppresses success, not ACK queue release. Malformed/impossible ACKs become explicit private errors, including parked-terminal input that would otherwise freeze retargeting. |
| `R/CopySender.cpp:409–439,476–581` | Native five-state copy lifecycle, context/no-replication mode, final-copy identity, delegated sender progress/latency, monotonically increasing target only while LSNSet, and no reopening ReplCompleted. Metadata already supplied by the enumeration owner replaces pointer-based IsOperationLast inspection; no second reliable sender or ordering queue is added. |
| `R/PrimaryReplicator.BuildIdleAsyncOperation.cpp:111–166`; `R/RemoteSession.cpp:367–385` | Invoke a fallible latest-issued getter from actual enumeration EOF, before sender Add and even when the terminal is parked. Propagate lookup failure as an operation error, not an invented provider fault. The native EOF recheck with copy LSN zero cannot satisfy its terminal predicate; no early replication gate is invented. This is the bounded callback, not GetCopyState/context acquisition or complete BuildIdle cleanup/StartCopy ownership. |
| `R/RemoteSession.cpp:585–615` | Actual session ACK-processor witnesses deliver replication sends, CopyReplicationGate, primary progress and CopyAck in native order. Existing handoffs remain caller-owned, not automatic production wiring. |
| `R/ReplicaManager.cpp:745–774,1983–2022`; `R/CopySender.cpp:87–103,555–581` | Port the upward pending-target update. Actual primary queue/session tests exercise EOF-issued versus committed progress and idle retention release after retarget. Barrier-synchronized real threads cover gate-versus-retarget serialization. Full timer/freshness/pressure eligibility, fault exclusion, notifications and promotion are future owners. |
| `R/SecondaryCopyReceiver.cpp:24–184,302–455`; `R/CopyContextReceiver.cpp:105–234` | Real receiver witnesses feed actual terminal ACKs into CopySender, without test-only sentinel normalization or a fake build-success flag. Required/optimistic service ACKs both wait for the real replication gate; context numeric terminal ACK needs no replication gate. |

There is no standalone native CopySender test file at this pin. The **47 new
tests** are production-source-derived Rust component vectors, not native harness
execution or a claim of differential parity. They exercise the actual wrapped
enumerator, queues, reliable senders, session handoffs and copy/context receivers.
Reentrant provider/getter/payload lifetimes, selected-work rooting, cancellation/
FinishOperation, capture/provider errors and the selected-success/disable race
are executable boundaries. End maps the native internal-error race to an
explicit private failure plus the existing runtime fault representation and
exact native description; no new protected/public error surface is introduced.

**Measured scope:** V1 **453**, runtime library **510/510** (57 unchanged non-V1),
workspace **868 = 822 ordinary + 41 PostgreSQL + 3 KinD + 2 build-only**.
Workspace check/Clippy, no-default runtime, API/privacy, doctests and exact
tier/partition gates passed. Discovery does not execute provider/cluster tiers.
See the [ledger](sf-v1-port-ledger.md#copysender-build-gates-slice-p3-bounded-f06f07f11)
and [test selector](testing.md#sf-v1-copysender-build-gates-slice).

Provider/context acquisition, reliable StartCopy identity/epoch establishment,
full BuildIdle/session/role/drain orchestration, periodic/slow-peer policy,
promotion and provider monitoring remain separate owners. All components stay
private/unselected. No dependencies, native Common/Transport/KTL utilities or
test harness, protected APIs, factory/service/wire/host/controller wiring,
native/differential/hosted/provider-fsync parity or full P2/P3 completion.

### Copy Establishment Slice (P3, Bounded F04/F05/F07/F08/F11)

Rust destinations retaining Microsoft copyright/MIT attribution:
[`copy_establishment.rs`](../../../kuberic-runtime/src/replicator/v1/copy_establishment.rs)
and its
[`tests.rs`](../../../kuberic-runtime/src/replicator/v1/copy_establishment/tests.rs),
with narrow changes to the existing session, logical carrier adapter and
enumeration configuration/error seams. `R/` below denotes
`src/prod/src/Reliability/Replication/` at the same immutable pin.

| Selected native production material | Incorporated responsibility and explicit Rust substitution |
|---|---|
| `R/RemoteSession.EstablishCopyAsyncOperation.cpp:30–61`; `R/RemoteSession.cpp:252–365`; `R/ReplicationSession.cpp:328–384` | Persisted incoming context created before StartCopy; reliable sender published before first send, replication-start LSN as operation identity, current sending epoch/infinite expiry, explicit timer and close. Existing rooted sender batches and logical SendIntent/tonic carrier replace native messages/Threadpool/ComponentRoot, not retry algorithms. Selected sends remain deliverable after close. |
| `R/PrimaryReplicator.cpp:550–705`; `R/RemoteSession.cpp:585–615,683–756` | First valid persisted target context vs nonpersisted successful ACK establishment; remote copy failure; actual context receiver processing and ordered session ACK handoffs. A caller-bound full Route validates endpoint/partition/replica/incarnation/address; it is not the native manager's idle-role lookup or dynamic session registration. Invalid shape/progress becomes explicit private error. Remote errors close retries even with no numeric ACK progress; first completion is preserved. |
| `R/SecondaryReplicator.cpp:915–1123` | Wrong replica/inactive/older epoch rejection, unexpected newer established epoch fault, once-only actual replication receiver initialization, duplicate ACK reissue, persisted target GetCopyContext and existing no-replication-gate CopySender. Caller-supplied force-ACK callback is a required scheduling handoff, not a transport/service ACK or host fault report. Full secondary lifecycle and cross-receiver drainage are not ported. |
| `R/PrimaryReplicator.BuildIdleAsyncOperation.cpp:84–166,185–214`; `R/ComProxyStateProvider.cpp:151–203` | Target context into protected source GetCopyState(start - 1); explicit acquisition fault descriptions and cleanup/cancellation unblocking of incoming context. Retained uniquely owned async provider futures replace synchronous COM acquisition while preserving one invocation across interrupted polls. Cancellation does not pretend an arbitrarily pending callback has drained; returned provider failure retains precedence. Successful acquisition begins the existing CopySender, not a synthetic build completion. |
| `R/ReplicaManager.cpp:1187–1193`; `R/ReplicationQueueManager.cpp:165–194` | The caller supplies the frozen classic committed boundary + 1. Its provider boundary remains distinct from the latest-issued completion target captured by existing CopySender at actual enumeration EOF. This slice does not implement manager admission, PC/CC boundary selection, pending-operation retention/replay or the V2 copy-until-latest capability. |
| `R/RemoteSession.cpp:312–324,733–756`; `R/SecondaryReplicator.cpp:862–913,1063–1123` | Explicit cancel/close/error cleanup, GetCopyState/GetCopyContext error identity, no fabricated EndGetNext fault. Weak handles avoid cycles; enumeration callbacks root establishment state; foreign polls/callbacks/wakes and payload retirement stay outside short guards. Private -1 error token preserves failure without claiming native HRESULT interoperability. Provider monitoring, health reporting and full cancellation/drain ownership remain pending. |

The **45 new registered tests** are production-source-derived actual Rust
component vectors, not imported/executed native fixtures. They cover real
session/reliable sender retries and selection, context receiver and copy sender
exchange into protected provider implementations, distinct provider/EOF targets,
ordered build-gate handoffs, identity/epoch/duplicate/invalid inputs, acquisition
failures, cancellation and interrupted driver lifetime. One real tonic
connection verifies StartCopy's serialized logical fields, not SF wire
interoperability. Root-reference tests supplement actual reentrant callbacks;
they do not substitute for native role/drain qualification.

**Measured scope:** V1 **498**, runtime library **555/555** (57 unchanged non-V1),
workspace **913 = 867 ordinary + 41 PostgreSQL + 3 KinD + 2 build-only**.
Workspace check/Clippy, no-default runtime, API/privacy, doctests and exact
tier/partition gates passed. See the
[ledger](sf-v1-port-ledger.md#copy-establishment-slice-p3-bounded-f04f05f07f08f11)
and [selector](testing.md#sf-v1-copy-establishment-slice).
Earlier slice counts and exclusions above remain historical boundary evidence.

The caller still owns idle-role eligibility, frozen-boundary selection, route
binding, acquisition/enumeration drivers, ordered ACK processing and remaining
timer delivery. Full BuildIdle/idle-session/primary/secondary/role/fault/drain
orchestration, slow-peer/promotion policy, provider monitoring and P4/P5
adapters/atomic activation remain pending. No protected interfaces, selected
production wiring, dependencies, native utilities/harness, complete P2/P3,
native/differential/hosted/provider-fsync parity or full ordinary execution claim.

### BuildIdle Lifecycle Slice (P3, Bounded F02/F05/F06/F07/F11)

Rust destinations retain Microsoft copyright/MIT attribution:
[`build_idle.rs`](../../../kuberic-runtime/src/replicator/v1/build_idle.rs)
and adjacent
[`tests.rs`](../../../kuberic-runtime/src/replicator/v1/build_idle/tests.rs).
Narrow integration changes extend the existing ReplicaManager, queue,
establishment, session timer and CopySender validation seams. `R/` denotes
`src/prod/src/Reliability/Replication/` at the same immutable pin.

| Selected native production source | Incorporated responsibility and explicit Rust substitution |
|---|---|
| `R/ReplicaManager.cpp:1142–1220`; `R/ReplicationQueueManager.cpp:165–194` | Fresh idle session creation/open with no caller progress, duplicate/identity checks, atomic frozen start and actual pending replication selection. Classic V1 uses max(current committed, previous-config quorum floor) + 1; V2 copy-until-latest is excluded. Manager serialization captures original operation roots/epochs and retention hint; pending replay dispatches outside the guard. |
| `R/PrimaryReplicator.BuildIdleAsyncOperation.cpp:42–166`; `R/RemoteSession.cpp:252–365` | Compose actual establishment, protected GetCopyState(start - 1), CopySender driver and End. Publish StartCopy during admission before pending replay can reenter/remove; dispatch replay before selected StartCopy. Rust short transitions/owned futures replace native Begin-End/Threadpool, preserving late-selected work while stopping retries and unsuccessful builds after removal. |
| `R/RemoteSession.cpp:585–615`; `R/ReplicationSession.cpp:285–326` | Consume every ordered ACK continuation, update the manager only at primary progress, and sample actual live EOF copy target for idle retention. Reuse CopySender normalization to reject impossible progress before sticky session coalescing. Explicit malformed-input errors replace native assertions; they do not imply successful ACK processing. |
| `R/PrimaryReplicator.BuildIdleAsyncOperation.cpp:84–166,178–214`; `R/RemoteSession.cpp:312–324,733–756` | Preserve BuildIdle context-only cancellation versus immediate session CancelCopy on removal/close. Cancellation is observed before provider acquisition; post-acquisition context cleanup does not invent unconditional copy cancellation. Retained async acquisition stays pending until actual completion, including after manager removal/close; provider failure keeps precedence. Actual GetCopyState/EndGetNext faults and first private primary-fault observation are distinct from host reporting. |
| `R/ReplicaManager.cpp:792–870,1783–1832` | Remove idle membership/retention constraint, close replication and cancel actual copy outside guards. Manager close stages terminal replicate waiters before build cancellation, including cleanup of other builds after an abandoned owner error. Per-session rooted handles prevent old-ID cleanup from affecting replacement builds. No synthetic success from removing quorum membership. |
| `R/RemoteSession.cpp:252–365`; `R/ReliableOperationSender.cpp:113–137,548–755`; `R/AckSender.cpp:64–208` | Caller-driven Tokio deadlines replace independent native timers. Establishment drives StartCopy while waiting; BuildIdle continues it during acquisition/enumeration, and drives pending-build replication/context ACK deadlines. Notify-based ingress/enqueue wakeups resample newly installed deadlines; stale StartCopy timer callbacks cannot alter the current retry window. No autonomous production task or alternate sender is introduced. |

The **41 new registered tests** execute actual Rust owners: 38 lifecycle cases
and three queue snapshot vectors. The previous-quorum-floor case supplies a
production-derived queue-state vector; it does not execute missing role
transfer. `R/ReplicaManager.Test.cpp:772–824` provides admission/helper ancestry;
the lifecycle/cancellation/fault/timer vectors derive from production sources,
not imported native test fixtures or executable formula ledgers.

**Measured scope:** V1 **539**, runtime **596/596**, unchanged non-V1 **57**.
Exact workspace discovery: **954 = 908 ordinary + 41 PostgreSQL + 3 KinD +
2 build-only**; ordinary slices **227/227/227/227**, PostgreSQL **11/13/8/9**.
Workspace check/Clippy, no-default/API/privacy, doctests and exact inventory gates
pass. See the [ledger](sf-v1-port-ledger.md#buildidle-lifecycle-slice-p3-bounded-f02f05f06f07f11)
and [selector](testing.md#sf-v1-buildidle-lifecycle-slice).

Registry metadata retains weak manager/provider-driver handles; dropping the
operation is abandonment, not successful drain. Successful builds remain idle;
the future primary coordinator owns ongoing ordinary session timers/ingress
after this build driver finishes. Full role/primary/secondary integration,
cross-receiver/provider drains, slow-peer/promotion policy, provider monitoring,
host health and P4/P5 adapters/activation remain pending. No protected/public
API, selected production path, dependencies, native utilities/harness,
native/differential/hosted/provider-fsync parity, full ordinary execution or
full P2/P3 completion claim.

### Secondary Lifecycle Slice (P3, Bounded F04/F05/F06/F09/F11)

Rust destinations retain Microsoft copyright/MIT attribution:
[`secondary.rs`](../../../kuberic-runtime/src/replicator/v1/secondary.rs)
and adjacent
[`tests.rs`](../../../kuberic-runtime/src/replicator/v1/secondary/tests.rs).
Narrow changes extend actual shared dispatch, receiver and establishment
seams, not substitute queues/provider algorithms. `R/` denotes
`src/prod/src/Reliability/Replication/` at the same immutable pin.

| Selected native production source | Incorporated responsibility and explicit Rust substitution |
|---|---|
| `R/SecondaryReplicator.cpp:393–550` | Retire admission, own ordinary Close/End and independent Abort, cancel actual context sender and preserve provider wait ordering. Explicit caller-driven futures/weak ingress replace COM/job ownership; dropping the owner is abandonment, not Close. |
| `R/SecondaryReplicator.CloseAsyncOperation.cpp:120–188,327–367` | Legacy ordinary Close aborts dispatch and clears pending queues, waits reserved UpdateEpoch but not held ACKs or GetCopyContext. No universal graceful-drain guarantee is inferred from native TODO comments. Promotion branch is excluded. |
| `R/SecondaryReplicator.CloseAsyncOperation2.cpp:116–421` | EOS ordinary Close clears unpumped replication before EOS on incomplete copy, cleans data without ignoring EOS ACK, aborts uninitialized streams, drains copy then replication, waits GetCopyContext then provider UpdateEpoch. Drain failure returns early; subsequent Abort still owns pending calls. Promotion branch is excluded. |
| `R/SecondaryReplicator.DrainQueueAsyncOperation.cpp:56–342`; `R/OperationStream.GetOperationAsyncOperation.cpp:139–184` | Observe real dispatch drain before required service ACK drain. Shared queue initialization/drain snapshots and outside-guard Notify wakes replace AsyncQueue waiters. Empty-open drain observation is not stream EOF. Stream faults preserve EOS wait and distinct failed drain, not fabricated success. |
| `R/SecondaryReplicator.cpp:1624–1965` | Reserve ACK-triggered epoch obligation before rooted callback completion; retain and invoke actual protected UpdateEpoch outside guards. Distinguish full ACK-barrier failure cleanup from stream-marker provider fault/report and resumed dequeue. Retained Rust provider futures replace native Begin-End/manual-reset events; no callback is fabricated on cancellation. |
| `R/SecondaryReplicator.cpp:862–914,1063–1123` | Actual GetCopyContext and EndGetNext provider failures record private typed faults. Acquisition additionally forces negative ACK only while copy remains in progress; enumeration provider fault is not a remote context failure. Non-provider remote errors use sticky negative context ACKs. This observation is not host health reporting. |
| `R/SecondaryReplicator.cpp:1967–2083,2235–2280`; `R/AckSender.cpp:64–208` | Actual copy completion checks context sender, ACK scheduling snapshots actual two-receiver progress, cross-receiver stream-fault cleanup preserves EOS ACK waits. Caller-driven Tokio deadlines replace timers; initialization/dequeue/ACK wakes resample pinned drivers. Cleanup tokens retain actual provider without receiver/root cycles. |

The **41 new registered tests** execute the actual Rust owners: 39 secondary
lifetime cases and two actual shared-dispatch lifecycle vectors. Test inputs
derive from the named production branches, not imported native test fixtures,
native binary execution or locally redefined state/formula ledgers. Pinned
paused-time tests cover ACK-deadline installation/live snapshots and continued
context retries; reentrant callbacks run outside guards. Pending context/epoch
calls retain actual errors after Close/Abort and interrupted polls.

**Measured scope:** V1 **580**, runtime **637/637**, unchanged non-V1 **57**.
Exact discovery: **995 = 949 ordinary + 41 PostgreSQL + 3 KinD + 2 build-only**;
ordinary slices **238/237/237/237**, PostgreSQL **11/13/8/9**. Workspace
check/Clippy, no-default/API/privacy, doctests and exact inventory gates pass.
See the [ledger](sf-v1-port-ledger.md#secondary-lifecycle-slice-p3-bounded-f04f05f06f09f11)
and [selector](testing.md#sf-v1-secondary-lifecycle-slice).

Typed data ingress assumes caller-owned route/epoch admission. The owner does
not implement role switching, promotion, external epoch reconfiguration, full
primary/session ownership, provider monitoring/health or P4/P5 adapters/cutover.
Neither Close nor Abort asserts arbitrary provider GetNext drainage. Legacy
containment, EOS initialized-stream drain, faults and Abort remain distinct;
uninitialized streams take native abort bypass. No protected API, selected
production behavior, dependencies, native utilities/harness,
native/differential/hosted/provider-fsync parity, full ordinary execution or
full P2/P3 completion claim.

### Primary Lifecycle Slice (P3, Bounded F02/F06/F07/F08/F09/F11)

Rust destinations retain Microsoft copyright/MIT attribution:
[`primary.rs`](../../../kuberic-runtime/src/replicator/v1/primary.rs) and
[`tests.rs`](../../../kuberic-runtime/src/replicator/v1/primary/tests.rs).
Narrow existing BuildIdle/establishment/manager seams are extended; ordered ACK
handoff delivery is shared, not reimplemented. `R/` denotes the same pinned
`src/prod/src/Reliability/Replication/` prefix.

| Selected native production source | Incorporated responsibility and explicit Rust substitution |
|---|---|
| `R/PrimaryReplicator.cpp:261–315`; `R/PrimaryReplicator.CloseAsyncOperation.cpp:36–132` | Ordinary optional pending-quorum prewait, timeout continuing Close, manager/catchup cancellation and selected callback drainage. Independent Abort skips prewait. Explicit caller-driven futures, rooted activity accounting and oneshot delivery replace native async jobs/callback queue; client waiter bodies are not callback dispatch. Creating-secondary branch is excluded. |
| `R/ReplicaManager.cpp:792–870,926–1007` | Actual existing manager cancels writes before session removal can invent quorum success, closes senders and cancels idle builds. Primary serializes admission/selection, delivers effects outside guards and distinguishes pending-quorum wait from selected-effect drainage. No arbitrary provider-build-call drain is asserted. |
| `R/PrimaryReplicator.cpp:550–728`; `R/ReplicaManager.cpp:2187–2222` | Bound endpoint/partition/incarnation/address lookup, idle-only context and copy ACK admission versus active replication-only ACKs. Current/previous membership lookup precedes retained idle membership. Existing caller-established CC/PC configuration remains a seam, not full native configuration/session creation or promotion. |
| `R/RemoteSession.cpp:448–656` | Actual rooted/coalesced session ACK processor continues ordered replication sends, actual copy replication gate, primary quorum progress and idle copy ACKs. Shared Rust handoff delivery drains all stages even on errors/reentrancy; selected work can finish after sender Close. |
| `R/RemoteSession.cpp:98–112,252–365`; `R/ReliableOperationSender.cpp:548–755` | Existing reliable sender remains authoritative. BuildIdle owns building timers; lasting primary lifetime owns ordinary replication retry/RequestAck after build completion and for caller-established sessions. Tokio Notify registration precedes deadline sampling; interrupted driver polls retain actual build/provider futures. No extra sender or alternate timer algorithm is added. |

The **42 new registered tests** execute actual components/provider
implementations using production-derived input vectors, not native binaries,
native test harness imports or locally redefined formula/state ledgers.
Post-build replication/retry/quorum, idle nonvoting behavior, receive/apply
RequestAck, CC/PC shared-root timers, reentrant/coalesced ACKs, paused deadlines,
removal/replacement identity, actual provider faults/retention and finite/
infinite prewait/Close/Abort/selected-effect drainage are exercised.

**Measured scope:** V1 **622**, runtime **679/679**, unchanged non-V1 **57**.
Exact discovery **1037 = 991 ordinary + 41 PostgreSQL + 3 KinD + 2 build-only**;
ordinary **248/248/248/247**, PostgreSQL **11/13/8/9**. Workspace check/Clippy,
no-default/API/privacy, doctests and exact inventory gates pass. See the
[ledger](sf-v1-port-ledger.md#primary-lifecycle-slice-p3-bounded-f02f06f07f08f09f11)
and [selector](testing.md#sf-v1-primary-lifecycle-slice).

At that slice boundary, native configuration/session
creation, role/external epochs, promotion/queue transfer, slow-peer/MustCatchup
policy, monitoring/host health and P4/P5 activation remained pending.
All owners remain private/unselected. No protected
API/selected wiring/dependency/tier change, native utility/harness import,
native/differential/hosted/provider-fsync parity, full ordinary execution or
full P2/P3 completion claim. Earlier slice omissions/counts remain historical.

### Primary Configuration Slice (P3, Bounded F02/F06/F08/F09)

Rust destinations retain Microsoft copyright/MIT attribution:
[`primary.rs`](../../../kuberic-runtime/src/replicator/v1/primary.rs),
[`configuration.rs` tests](../../../kuberic-runtime/src/replicator/v1/primary/tests/configuration.rs)
and narrow existing manager/session/sender seams. `R/` denotes the same pinned
`src/prod/src/Reliability/Replication/` prefix.

| Selected native production source | Incorporated responsibility and explicit Rust substitution |
|---|---|
| `R/PrimaryReplicator.cpp:340–462` | Distinct catch-up/current-only updates, primary fault rejection and actual outstanding catchup reevaluation. Typed private Rust configurations contain remote active members; no COM/host descriptor ABI or public method change. |
| `R/ReplicaManager.cpp:306–335,1205–1242` | ID-based reuse, real session creation/Open seeded by descriptor progress, actual retained replay starting at last apply + 1. Optional typed progress substitutes for FABRIC_INVALID_SEQUENCE_NUMBER. Reused identity/progress is not overwritten; bound ACK ingress remains full-identity checked. |
| `R/ReplicaManager.cpp:1244–1779` | Existing manager supplies CC/PC/idle reuse, independent quorums, catchup boundary/marks, unknown-current admission, PC contraction, current replay requirement, explicit previous-only replay pause and unreferenced-sender retirement. Candidate validation/installation is failure-atomic; binding publication precedes outside-guard replay and reentrant callbacks. |
| `R/ReplicaManager.cpp:1738–1779`; `R/ReplicationSession.cpp:279–283` | Actual idle-to-active session marking and reliable sender average reset, including PC/idle overlap. Real sender/build roots and live latency watches remain. No copy-success assertion, synthetic cancellation/drain, secondary-to-primary role transition or slow-peer policy is introduced. |

The **34 registered tests** execute actual primary/manager/session/build/provider
components, not native binaries, native harness imports or locally redefined
formula ledgers. They exercise creation/seed/replay/original epochs, reuse versus
bound identity, overlap/contraction/retirement, unknown progress, failure atomicity,
actual idle session promotion/averages and pending provider retention. Actual
quorum/catchup reevaluation, reentrant ACK/reconfiguration/Abort, selected-send
Close drainage, queue-full retry and pinned-driver deadlines are also exercised.
Power-of-two sender windows and retry/decay settings are validated before owned
session construction; malformed typed input returns explicit private errors.

**Measured scope:** V1 **656**, runtime **713/713**, unchanged non-V1 **57**.
Exact discovery **1071 = 1025 ordinary + 41 PostgreSQL + 3 KinD + 2 build-only**;
ordinary **257/256/256/256**, PostgreSQL **11/13/8/9**. Workspace check/Clippy,
no-default/API/privacy, doctests and exact inventory gates pass. See the
[ledger](sf-v1-port-ledger.md#primary-configuration-slice-p3-bounded-f02f06f08f09)
and [selector](testing.md#sf-v1-primary-configuration-slice).

All owners remain private/unselected. Role/external-epoch orchestration,
secondary-to-primary promotion/drained queue transfer, periodic/slow-peer policy,
monitoring/host health and P4/P5 activation remain pending. No protected API,
selected wiring, dependency/tier change, native utility/harness import,
native/differential/hosted/provider-fsync parity, full ordinary execution or
full P2/P3 completion claim. Earlier slice omissions/counts are historical.

### P1 documented expectations (historical foundation)

`R/` is `src/prod/src/Reliability/Replication/`. The
[ledger](sf-v1-port-ledger.md#deterministic-scenario-inventory) records each
scenario's source ranges and immutable links. The listed selected native files'
copyright headers identify Microsoft MIT. This register covers documented native
test expectations, not just production algorithm descriptions. The scenarios
are acceptance criteria for future actual component tests, not translated
executable tests or checks of a locally redefined formula.

| Material selected / vectors | Exact native test source and case where translated | Transformation / what is not incorporated |
|---|---|---|
| Enqueue/assignment and API error A1 | `R/PublicApi.Test.cpp:454–504`, `BeginReplicateReturnsSOKIfAsyncOperationFails`; production `ReplicationQueueManager.cpp:369–408`, `ReplicaManager.cpp:349–390` | Preserve Begin S_OK / End NotPrimary expected split; chosen numeric progress10/11/full12 from production, not native test constants |
| Gap/contiguous queue Q1 | `R/OperationQueue.Test.cpp:1049–1095`, `TestOutOfOrderOperations1/2` ancestry | Document a fixed receive2,1 / ACK2,1 expectation reduced from randomized 1024-item permutations; no queue, predicate implementation or random/Boost fixture copied |
| Memory retention Q2 | `R/OperationQueue.Test.cpp:435–461`, `TestEnqueueOperationsWithMemoryLimit`; production `OperationQueue.cpp:256–318` | Selected 4×100, retained100, reject301/accept100 assertions; production-derived add300 neighbor evicts completed100 and totals600; sufficient count growth, no pending-suffix drop; no C++ queue/harness |
| Commit-before-complete Q3 | `R/OperationQueue.Test.cpp:607–615`, `TestCompleteBeforeCommit` | Selected false/true assertions; empty retained capability100→0 is separate production-derived scenario |
| Secondary trim Q4 | `R/OperationQueue.Test.cpp:867–901`, `TestSecondaryQueueTrim`; duplicate production `OperationQueue.cpp:371–399` | Selected completed-head sequence and count/history limits; no ring/allocator implementation |
| Quorum M1 | `R/ReplicaManager.Test.cpp:154–220`, `TestUpdateConfigurationWithQuorumIgnorePreviousConfig`, `TestUpdateConfigurationNoQuorumIgnorePreviousConfig` | Selected remote progress/configured W and committed/completed values; no sessions/network helper |
| Receive/apply R1 | `R/ReliableOperationSender.Test.cpp:102–143`, `TestReceiveACKProgress` | Retain fixed ACK pair/pending-list assertions and empty timer condition; no native sender/timer implementation |
| Full-copy cancellation C3 | `R/CopyAsyncOperation.Test.cpp:418–449`, `CopyAsync_QueueFull_Cancelled`; production `CopyAsyncOperation.cpp:42–49`, `SecondaryCopyReceiver.cpp:388–395` | Native 15/15/4 and cancel-at12 identified; P1 reduced full/parked/cancel outcome, not complete replay; copy count limits with no maximum-memory limit |
| Demux T1 | `R/ReplicationTransport.Test.cpp:594–617`, `TestDropMessageToNotMatch` | Routing assertions only; remove sockets and Sleep(100); incarnation scenario derived separately from production session lookup |
| Batched health H3 | `R/BatchedHealthReporter.Test.cpp:107–135`, `TestWarningsWithinIntervalReport` | Temp2/latest warning and total2 after Close preserved; timer fire is named input, not sleep or SF report sink |
| Production-derived scenarios M2–M5, S1–S3, R2–R3, C1–C2, C4–C6, T2, D1, L1–L3, E1–E2, H1–H2 | Exact production paths/ranges in the ledger: queue/manager, secondary/receiver/stream, session/sender/ACK, copy/establishment, facade/settings, headers, RA lifecycle/access | Documented deterministic inputs and expected observations; not executable formula checks, wholesale native tests or executed state machines |
| Capability/settings cases | `ComReplicator.h:12–35`; `ComReplicator.cpp:53–83,490–808,1002–1118`; `ComOperationStream.h:12–24`, `.cpp:48–78`; `OperationStream.cpp:66–85`; `reconfig.h:12–17`; `src/prod/src/ServiceModel/reliability/replication/REConfigMacros.h:136–207`; public `FabricTypes.idl:2910–2960` | Classification/default/update facts including legacy flags/EX3 precedence; managed Buffer/Count validation, no attributed Offset bounds check; no COM ABI, credentials implementation or settings parser copied |
| Protected resolved fixtures | Existing Kuberic `application.rs:31–37`, protected traits and external positive/privacy infrastructure | Kuberic-authored compiler identities and trait implementations; no native segmented codec or COM implementation |

### Reviewed-only context, not whole-test translations

- `R/ReplicationSecondary.Test.cpp:529–579,1450–1507,1550–1587`:
  progress/drain mode context, not executed P1 native progress tests.
- `R/ChangeRole.Test.cpp:25–45`: role test registrations, not copied network run.
- `R/ReplicateCopyOperations.Test.cpp:708–741,1093–1105,1455–1467`:
  empty-context and enabled-EOS context, not a translated full copy test.
- `R/ReplicatorSettings.Test.cpp:706–795`: config-store dynamic notifications;
  not evidence that arbitrary public settings updates succeed.
- `src/prod/src/ServiceGroup/Service/ServiceGroup.StatefulService.h:153–180`
  and `ServiceGroup.StatefulServiceUnit.h:27–58`: atomic wrapper boundary only.
- `R/ComProxyStateProvider.cpp:19–26` and
  `src/prod/src/data/txnreplicator/ComStateProvider.h:10–29`: distinct internal
  copy-until-latest marker; no V2 implementation incorporated.
- `R/test/CMakeLists.txt:8–30,37–64`: native dependency/target inspection only;
  no native build configuration or dependencies imported.

## Third-party boundary and future additions

Native
[`ThirdPartyNotices.txt`](https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/ThirdPartyNotices.txt)
is **heterogeneous**, not an all-MIT manifest. For example, its opening entries
include jQuery/Sizzle notices, moby-derived Apache-2.0 code and Boost.Asio under
the Boost Software License. Root MIT alone does not license every vendored file.

The selected Microsoft production/test assertion material above was reviewed
for its own headers/origin. No third-party utility, vendored asset, Boost test
framework, Common/Transport/KTL source or native dependency build graph is
incorporated by P1 or these bounded slices. Linking of the *upstream* tests to those dependencies does
not mean those sources were translated here. Conversely, a future selection of
such material requires checking its exact origin/license and preserving every
applicable notice. Do not blindly copy all unrelated umbrella notices, omit
applicable notices, or assume an include implies an all-MIT dependency closure.

Every future translated component/test must extend this register with pin,
path/range/case, Rust destination, transformations, retained copyright/license,
third-party selections and actual execution/review status. This is scoped
source/provenance evidence, not an all-distribution legal certification.
