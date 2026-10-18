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
