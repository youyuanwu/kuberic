# SF V1 Source Provenance and Notices

## Selected upstream and license

Selected production algorithms, message contracts and test expectations derive
from Microsoft Service Fabric at immutable commit
[`3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d`](https://github.com/microsoft/service-fabric/tree/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d).
Copyright (c) Microsoft Corporation. All rights reserved.

The native root file is
[`LICENSE`](https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/LICENSE),
**not `License.txt`**, despite some upstream comments. Its complete Microsoft
MIT permission, inclusion requirement and disclaimer are retained verbatim in
[`LICENSES/service-fabric-MIT.txt`](../../../LICENSES/service-fabric-MIT.txt).
See [`THIRD-PARTY-NOTICES`](../../../THIRD-PARTY-NOTICES).
Kuberic's root license and adapted API-documentation notices are unchanged.

This register records **ancestry and transformations**, not delivery progress.
Kuberic selected-host Close/access evidence is assessed at
`aad23335c219a576d69133cee883bbd63e40b68a`; unchanged component ancestry
retains its original explicit anchors. See
[current semantics](classic-v1-runtime.md) and the
[evidence ledger](sf-v1-port-ledger.md). No native binary or native test was
run for this source-guided work. Named native cases below mean selected
expectations, not whole native fixtures or an executable compatibility oracle.
Production-derived Rust vectors remain distinct from translated test assertions.

### Path conventions

- **R/** =
  [`src/prod/src/Reliability/Replication/`](https://github.com/microsoft/service-fabric/tree/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Replication)
  at the immutable pin above. Unprefixed native replication filenames use R/.
- **RA/** =
  [`src/prod/src/Reliability/Failover/ra/`](https://github.com/microsoft/service-fabric/tree/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Failover/ra).
- Other native paths are repository-root-relative at that pin. Ranges are native
  source lines, not Kuberic line numbers.
- Rust destinations below start at
  [`kuberic-runtime/src/replicator/v1/`](../../../kuberic-runtime/src/replicator/v1/)
  unless an explicit repository path is given. “Adjacent tests” means that
  component's `tests.rs` or `tests/` module. Headers retain selected Microsoft
  copyright/MIT attribution.

## Component material register

Tables preserve selected native source/test ranges, named cases, Rust
destinations and transformations. They do not assert every native branch,
public capability or interleaving is qualified. The default production adapter
composes the private engine components
([production.rs:127–227](../../../kuberic-runtime/src/replicator/v1/service_adapter/production.rs#L127-L227)).
The private native-shaped host is not wholly selected. Production now has
its own bounded operation-owned stage/outer-Close alignment; see
[current semantics](classic-v1-runtime.md#host-access-lifecycle-and-retirement).

### Queue — F03

Destinations: [`operation_queue.rs`](../../../kuberic-runtime/src/replicator/v1/operation_queue.rs)
and [adjacent tests](../../../kuberic-runtime/src/replicator/v1/operation_queue/tests.rs).

| Selected native material | Translation / exclusions |
|---|---|
| `OperationQueue.h:21–188`; `OperationQueue.cpp:26–180,194–203,214–236` | Owned synchronous queue, configuration, four cursors, transfer/callback installation; `Arc`/`Bytes` replace COM references/data access. Atomic needs-ACK predicate is a dependency, not a second receiver. |
| `OperationQueue.cpp:256–668,670–987,990–1306,1347–1389` | Admission/span/count/byte limits, resize/convergence, contiguous commit/complete, retention/trim, rewind/discard/reset. Ordered map replaces ring allocation, preserving positional capacity; ring aliasing outside retention is not an API. |
| `OperationQueue.Test.cpp:403–426,435–605,607–615,867–901,995–1095` | Selected growth/history/memory/gap/commit/trim assertions; deterministic arrivals replace random fixtures. Earlier-hole case uses actual 598-byte call at490–493, not neighboring stale comment. |
| Production `OperationQueue.cpp:93–180,214–236,677–987,1128–1306` | Additional ACK-required/callback call-point/self-disable/replay/target-prerequisite/transfer/reset/lifetime vectors; not native queue unit tests of ACK-required mode. |

Callback progress is an immutable native call-point snapshot with keep/disable
return instead of arbitrary reentrant mutation. Ordinary commit calls back
before advancing commit; ignore-commit completion advances commit first.
Lifecycle timestamps/trace/performance/description sinks are omitted; resize
averaging is algorithmic, computed rather than cached. Representable
LSN/capacity and allocatable storage are prerequisites, not an extreme-number
or allocator-recovery guarantee.

### Sender, session and ACK — F06

Destinations: [`sender.rs`](../../../kuberic-runtime/src/replicator/v1/sender.rs),
[`session.rs`](../../../kuberic-runtime/src/replicator/v1/session.rs),
[`ack.rs`](../../../kuberic-runtime/src/replicator/v1/ack.rs) and adjacent tests.

| Selected native material | Translation / exclusions |
|---|---|
| `ReliableOperationSender.h:158–178,209–235`; `ReliableOperationSender.cpp:25–205,207–472,509–823` | Sender state, single/replay merge, receive/apply, hints/window/retry/rearm and timer-only RequestAck. Existing rooted queue entries and selected batches replace raw pointers; injected relative time has explicit never-selected state. No second queue/LSN allocator/byte window. |
| `ReliableOperationSender.cpp:899–985`; `REPerformanceCounters.h:12–20`; `DecayAverage.cpp:19–119` | Admission watches, positional ACK traversal, signed tick means/strict collection/decay/saturation/reset; safe iteration does not copy native iterator UB. Sparse-latency domains and dynamic config-store/trace sinks are not certified. |
| `AckSender.h:15–62`; `AckSender.cpp:24–225` | Batch/force/pending-only final flush, force gate, issuance/trace; rooted callback rather than payload. Selected timer callbacks may outlive Close; Close is not join. Trace stopwatch persists across reopen. |
| `RemoteSession.cpp:33–169,434–446,448–655,757–790`; `ReplicationSession.cpp:278–325,426–463` and header declarations | Session identity/epoch/progress, coalesced ordered ACK work, replication/RequestAck and expiry. Short transitions/move-only rooted handoffs replace posting; apply sampled after sender dispatch, finish/recheck atomic with ingress, epoch sampled per send. No invented RequestAck payload. |
| `RemoteSession.cpp:99–112,243–259,591–595` | Close/open and establishment-owned StartCopy sender seam; selected work survives Close and post-close ACK processing is not categorically rejected. |
| `ReliableOperationSender.Test.cpp:102–143` **TestReceiveACKProgress**; `ReliableOperationSender.Test.cpp:145–199` **TestAddRemove**; `ReliableOperationSender.Test.cpp:201–236` **TestAddMoreThanContinuousSend**; `ReliableOperationSender.Test.cpp:238–310` three **TestMaxSendingWindowSize** cases | Selected actual sender/queue assertions; manual time/deterministic arrivals replace sleeps, shuffle and racing-timer tolerance. |
| `ReliableOperationSender.Test.cpp:312–445` **TestOperationLatencyList** and helpers; `DecayAverage.Test.cpp:28–63` **NoDecay_Basic**, **NoDecay_Overflow**, **SimpleAverage**, **SimpleAverageOverflow** | Separate stopwatch/collection/latest/running-average/saturation expectations; dense small cases replace random100-item fixtures. |
| `ReliableOperationSender.cpp:339–472,548–787`; `AckSender.cpp:99–225`; `RemoteSession.cpp:492–655` | Additional replay/time/hint/backpressure/decay/flush/close/reopen/pair-replacement/flags/reentrancy/handoff vectors, not dedicated native AckSender/RemoteSession unit tests. |

Reviewed caller/time context (not imported utility owners):
`ReplicaManager.cpp:107–147,1138–1242,1333–1345,1835–1925`;
`PrimaryReplicator.cpp:632–750`; `CopySender.cpp:56–85,267–406`;
`CopyAsyncOperation.cpp:42–125,237–335,367–378`;
`CopyContextReceiver.cpp:80–127,154–237`;
`SecondaryReplicator.cpp:681–824,1214–1261,2099–2107`;
`ReplicateCopyOperations.Test.cpp:744–878`;
`CopyAsyncOperation.Test.cpp:276–303,352–449`;
`src/prod/src/Common/Timer.h:33–56`,
`src/prod/src/Common/Timer.Linux.cpp:452–489`,
`src/prod/src/Common/Stopwatch.cpp:49–115`,
`src/prod/src/Common/TimeSpan.h`,
`src/prod/src/Common/AsyncOperation.cpp:150–195`.

### Carrier, buffers and async execution — F05/F08/F11

Destinations: [`transport.rs`](../../../kuberic-runtime/src/replicator/v1/transport.rs),
[`buffers.rs`](../../../kuberic-runtime/src/replicator/v1/buffers.rs),
[`async_runtime.rs`](../../../kuberic-runtime/src/replicator/v1/async_runtime.rs),
[`sf_v1.proto`](../../../kuberic-runtime/proto/sf_v1.proto) and adjacent tests.

| Selected native material | Translation / exclusions |
|---|---|
| `ReplicationEndpointId.h:23–80`; `ReplicationTransport.cpp:919–937` | Full partition/replica/incarnation/address routing; bound routes/supersession replace actor transport, not native authorization/hosting. |
| `ReplicationTransport.cpp:168–266,268–355,684–878`; `ReplicationOperationHeader.h:25–110`; `StartCopyMessageBody.h:17–47` | ACK/error, original/sending epochs, segments/batches/terminal markers/StartCopy/RequestAck/InduceFault. Bidirectional tonic logical messages are not native wire format; acceptance is not service ACK. |
| `ReplicationTransport.cpp:880–937`; `RemoteSession.cpp:757–790` | Retry/infinite expiry, queue-full-only batch stop, too-large/failure and shared-header construction obligations; bounded Rust queue/outcome adaptation, not native transport implementation. |
| `ReliableOperationSender.cpp:113–137,548–755`; `AckSender.cpp:64–208`; `RemoteSession.cpp:448–655` | Tokio clocks/timers drive existing algorithms with outside-lock effects and rooted final ACK/handoffs; no Common timer/cancellation implementation. |
| `src/prod/src/idl/public/FabricRuntime.idl:626–683`; `src/prod/src/managed/Api/src/System/Fabric/OperationData.cs:16–38` | Ordered buffer arrays inform raw/framed conversion through protected bytes. Versioned framing/checked decoder are Kuberic-authored, not native serialization. |
| `ComOperation.cpp:100–115`; OperationQueue byte admission/history/removal paths | Logical payload accounting excludes Rust framing overhead; actual queue vectors exercise suffix displacement/rewind/trim/discard. |
| `ReplicationTransport.Test.cpp:594–617` **TestDropMessageToNotMatch**; production above | Bound-route/incarnation assertions over local tonic, not native sockets/sleep fixture; other runtime/frame vectors are production-derived. |

Runtime effect staging has a separate fixed64-entry adapter limit and reserves
final ACK capacity, not a native REConfig default. Capacity errors precede
mutation. Explicit runtime shutdown joins selected work; native Close does not
reject all rooted ACK work. Carrier client/server futures require caller
ownership; abnormal unjoined Drop is not graceful drain.

### Primary queue and quorum — F02/F06/F09

Destinations: [`replication_queue_manager.rs`](../../../kuberic-runtime/src/replicator/v1/replication_queue_manager.rs),
[`replica_manager.rs`](../../../kuberic-runtime/src/replicator/v1/replica_manager.rs)
and adjacent tests.

| Selected native material | Translation / exclusions |
|---|---|
| `ReplicationQueueManager.cpp:29–91,99–160,209–236,261–408` and header | Construction/normalization, issued/committed/capability, configuration capture, projection and admission-only assignment. Existing roots preserve original epoch/logical bytes; None represents NonInitializedLSN, not missing-input success. |
| `ReplicaManager.cpp:349–447,1061–1103,1244–1374,2328–2525` | PC/CC quorum, receive reclamation, all-current apply, W1 asymmetry, catch-up cap and MustCatchup. Concrete sessions replace native descriptor/session execution plumbing. |
| `ReplicaManager.cpp:107–147,792–869,2076–2197,2266–2275,2351–2411` | Admission/separate completion, multicast/replay/terminal staging; caller serialization and outside-owner effects replace COM/job queues, not service commit. |
| `PrimaryReplicator.CatchupAsyncOperation.cpp:43–104`; `PrimaryReplicator.cpp:383–419,761–832` | Single pending owner; moving All, captured WriteQuorum and current-Q-relative marked minimum; dropping observer is not rollback. |
| `ReplicaManager.Test.cpp:154–220` **TestUpdateConfigurationWithQuorumIgnorePreviousConfig**, **TestUpdateConfigurationNoQuorumIgnorePreviousConfig** | Selected real-manager/session expectations; other atomicity/ACK/admission vectors derive from production, not a native transport fixture or second formula oracle. |

### Secondary operations, streams and ACK — F04/F05/F06/F09

Destinations: [`received_operation.rs`](../../../kuberic-runtime/src/replicator/v1/received_operation.rs),
[`operation_stream.rs`](../../../kuberic-runtime/src/replicator/v1/operation_stream.rs),
[`secondary_receiver.rs`](../../../kuberic-runtime/src/replicator/v1/secondary_receiver.rs)
and adjacent tests.

| Selected native material | Translation / exclusions |
|---|---|
| `ComFromBytesOperation.h`; `ComFromBytesOperation.cpp:127–257`; `ComOperation.h` metadata/NeedsAck | Five-state callback lifetime, ordinary cancel, EOS roots/late ACK; `Arc`/mutex/move-only work replaces COM. ReadyToRun blocks queue, Running releases predicate; completion/drop outside guard. Original selected callback identity survives cleanup, foreign same-LSN callback cannot release it. |
| `SecondaryReplicationReceiver.h`; `SecondaryReplicationReceiver.cpp:157–224,250–343,345–444,632–749` | Real queue heads/gaps/LSN duplicates/pressure/hints; original metadata/root retained. Receive differs from service completion; volatile default completes at dispatch. |
| `SecondaryReplicationReceiver.cpp:446–630`; `ComUpdateEpochOperation.h` | Original-epoch establishment, stream marker vs full required-ACK barrier, captured prior-epoch boundary and callback disable/replay; failed/abandoned handoff is not successful Close. |
| `OperationStream.h`; `OperationStream.cpp`; `OperationStream.GetOperationAsyncOperation.h`; `OperationStream.GetOperationAsyncOperation.cpp`; `ReplicationPointers.h:33` | Shared dispatch/barrier, open wait vs EOF/EOS; futures/wakers replace Begin-End/ReaderQueue. Sticky abort error once per handle, then EOF and cancellation-safe broadcast are explicit Rust policies, not COM error-result parity. |
| `SecondaryReplicator.cpp:165–193,324–350,675–679,1306–1396,1624–1701,2087–2108`; `Constants.cpp:15–19` | Persisted-or-required mode, optimistic callback omission, sending-epoch admission, ACK progress/threshold; real copy pending count feeds AckSender. InvalidLSN0 differs from NonInitializedLSN-1. |
| `ReplicationSecondary.Test.cpp:1951–2010` **OutOfOrderReplication**, **OutOfOrderReplicationNoReply**, **OutOfOrderReplicationExceedingQueueCapacity**, **DuplicateReplication**; `2052–2105` **MultipleOperationEpochs**, **VerifyStateProviderEstablishedEpochTest** | Deterministic actual-owner ordering/duplicate/capacity/epoch assertions replace native wrappers/random ACKs/sleeps. |
| `SecondaryReplicator.cpp:1740–1860,2247–2286` | Fault/two-receiver cleanup/drain context; receiver-local containment alone is not full owner lifecycle. |

### Copy receivers and context stream — F04/F05/F07

Destinations: [`secondary_copy_receiver.rs`](../../../kuberic-runtime/src/replicator/v1/secondary_copy_receiver.rs),
[`copy_context_receiver.rs`](../../../kuberic-runtime/src/replicator/v1/copy_context_receiver.rs),
[`copy_receiver_integration.rs`](../../../kuberic-runtime/src/replicator/v1/copy_receiver_integration.rs),
received-operation constructor and adjacent tests.

| Selected native material | Translation / exclusions |
|---|---|
| `SecondaryCopyReceiver.h:40–54,72`; `SecondaryCopyReceiver.cpp:24–184,186–285,302–455` | Count-only ordering/ACK/last/gap/full/duplicate/copyDone/sentinels/close/EOS; real shared queue/callbacks replace pointers. Preserve legacy terminal/wait predicate rather than guessing it away. |
| `CopyContextReceiver.h:12–85`; `CopyContextReceiver.cpp:27–63,80–97,105–145,147–234` | Target-context receive, no-service-ACK queue, terminal bookkeeping, strict threshold and close/final-ACK ordering. Small snapshot/close Arcs avoid native root cycles. |
| `ComOperationDataAsyncEnumerator.cpp:59–75,90–157,188–196` | Data-only consuming stream; successful close allows pending dequeue, later calls fail; error close fails pending. Shared close/error-once-then-EOF and immediate inaccessible-data retirement are Rust policies. |
| `ReplicationTransport.cpp:722–752,776–802`; `ComFromBytesOperation.cpp:11–24`; `Constants.cpp:15–19` | Positive protocol terminal LSN, invalid epoch(0,0), distinct zero-LSN local EOS; logical bytes, not framing overhead, classify terminal. |
| `AckSender.cpp:98–208`; `ReplicationSession.cpp:466–509`; `SecondaryReplicator.cpp:681–825,1500–1570,1967–2055,2087–2108`; `RemoteSession.cpp:585–627` | Actual combined pending count/current-state ACK and ordered handoff; callbacks/wakes/retirement outside guard. Not a fabricated provider/build/drain success. |
| `ReplicationSecondary.Test.cpp:1726–1911`; `ReplicateCopyOperations.Test.cpp:430–438,744–807,1049–1108,1972–2013` | Selected order/duplicate/capacity/last/zero-context/batching/EOS expectations, deterministic time/input not native fixtures. |

### Enumeration and CopySender — F05/F06/F07/F11

Destinations: [`copy_async_operation.rs`](../../../kuberic-runtime/src/replicator/v1/copy_async_operation.rs),
[`copy_sender.rs`](../../../kuberic-runtime/src/replicator/v1/copy_sender.rs)
and adjacent tests; existing sender's read-only pending view stages releases.

| Selected native material | Translation / exclusions |
|---|---|
| `CopyAsyncOperation.h:12–103`; `CopyAsyncOperation.cpp:42–60,70–212` | Count-only clean-on-complete/ignore-commit queue from1, serial enumeration, full/EOF/inflight cancellation and provider-error precedence. Caller-driven future replaces posting/Begin-End, not provider acquisition. |
| `CopyAsyncOperation.cpp:214–271` | One parked item, next assignment, numbered EOF, last callback once before sender Add even if parked; empty bytes are data. |
| `CopyAsyncOperation.cpp:273–365,367–417` | Receive dispatch before applied completion/parked retry; wake not inline poll; separate idempotent End/provider-fault/sender views. Ready is not invented Build success. |
| `ComProxyAsyncEnumOperation.cpp:90–121,137–195` | Nonnull empty item vs null EOF; absent enumerator becomes explicit empty Rust stream. Weak handles/unique stream lease retain provider root through End and interrupted polling; Drop is disposal ownership, not optional COM interface parity. |
| `CopyAsyncOperation.Test.cpp:276–303,320–449` | Cancellation timing, twelve-item success, fifteen-item/eight-slot/ACK-below-four full retry and smaller parked/EOF vectors. Invalid inputs, timer/framing/reentrancy cases are production/Rust-derived, not native harness execution. |
| `SecondaryCopyReceiver.cpp:24–184,302–455`; `CopyContextReceiver.cpp:105–234` | Actual receiver boundaries preserve required/optimistic ACK, ordinary empty data and terminal identity; no test-only Build success. |
| `CopySender.h:29–157`; `CopySender.cpp:26–265` | One-shot Begin/stale/prefailed completion/FinishOperation/EOF/End/disable race; caller driver, short transitions and callback root replace COM; first completion wins. |
| `CopySender.cpp:267–406` | Separate replication gate/final copy completion; normalize copy-1 to known terminal, preserve zero/no-update; disabled success still releases ACK queue. Malformed parked-terminal progress explicitly rejects. |
| `CopySender.cpp:409–439,476–581` | Five states, context/no-replication mode, terminal identity, delegated sender views; increase target only while LSNSet, never reopen ReplCompleted. |
| `PrimaryReplicator.BuildIdleAsyncOperation.cpp:111–166`; `RemoteSession.cpp:367–385` | Fallible latest-issued getter at actual EOF before Add, even parked; lookup error is not invented provider fault. Native zero-copy-LSN EOF recheck cannot satisfy terminal predicate. |
| `RemoteSession.cpp:585–615` | Real ordered sends → copy replication gate → primary progress → copy ACK handoff. |
| `ReplicaManager.cpp:745–774,1983–2022`; `CopySender.cpp:87–103,555–581` | Upward pending retarget; actual queue/session retention and gate/retarget thread witnesses. Timer/pressure policy is a separate responsibility below. |

There is no standalone native CopySender test file at this pin. Its Rust vectors
derive from production; selected-success/disable failure maps to a private error
and existing runtime fault, not a new protected API. Unique mutable enumeration
driver/stream lease is the serialization contract; no guard crosses foreign
polling and retired payload/waker drops occur outside transitions.
`CopySender.cpp:56–85,267–406,524–579` and
`PrimaryReplicator.BuildIdleAsyncOperation.cpp:111–166` also provide reviewed
enumerator boundary context. Native ApiMonitoringWrapper is not imported.

### Establishment and BuildIdle — F02/F04/F05/F06/F07/F08/F11

Destinations: [`copy_establishment.rs`](../../../kuberic-runtime/src/replicator/v1/copy_establishment.rs),
[`build_idle.rs`](../../../kuberic-runtime/src/replicator/v1/build_idle.rs)
and adjacent tests, with actual manager/session/queue/carrier seams.

| Selected native material | Translation / exclusions |
|---|---|
| `RemoteSession.EstablishCopyAsyncOperation.cpp:30–61`; `RemoteSession.cpp:252–365`; `ReplicationSession.cpp:328–384` | Persisted context before StartCopy, rooted sender before first send, start-LSN identity/epoch/infinite expiry/timer/close. Logical carrier replaces messages/Threadpool, not retry algorithm. |
| `PrimaryReplicator.cpp:550–705`; `RemoteSession.cpp:585–615,683–756` | First valid persisted target context vs nonpersisted ACK establishment, remote error and ordered handoffs; full caller-bound route checks, not invented manager lookup. |
| `SecondaryReplicator.cpp:915–1123` | Identity/epoch/active rejection, newer-established-epoch fault, once-only receiver init, duplicate ACK, persisted target context and no-replication-gate context sender. |
| `PrimaryReplicator.BuildIdleAsyncOperation.cpp:84–166,185–214`; `ComProxyStateProvider.cpp:151–203` | Target context to source GetCopyState(start-1); one retained acquisition across interrupted polls, actual error precedence and context cleanup. Pending arbitrary callback is not drained by cancellation. |
| `ReplicaManager.cpp:1187–1193`; `ReplicationQueueManager.cpp:165–194` | Frozen classic committed start differs from EOF latest-issued completion target; no V2 copy-until-latest. |
| `RemoteSession.cpp:312–324,733–756`; `SecondaryReplicator.cpp:862–913,1063–1123` | Cancel/close/GetCopyState/GetCopyContext error identities; no fabricated EndGetNext failure. Private-1 token is not HRESULT interoperability. |
| `ReplicaManager.cpp:1142–1220`; `ReplicationQueueManager.cpp:165–194` | Fresh idle/no-progress session, frozen max(current committed, previous quorum floor)+1, actual original roots/pending replay; serialized capture, outside-guard dispatch. |
| `PrimaryReplicator.BuildIdleAsyncOperation.cpp:42–166`; `RemoteSession.cpp:252–365` | Actual acquisition/sender/End; publish StartCopy during admission before reentrant removal, dispatch replay before selected StartCopy. Late selected sends may survive removal, retries/build success may not. |
| `RemoteSession.cpp:585–615`; `ReplicationSession.cpp:285–326` | Every ACK continuation, manager update at actual primary progress, live EOF target retention; normalize before sticky coalescing. |
| `PrimaryReplicator.BuildIdleAsyncOperation.cpp:84–166,178–214`; `RemoteSession.cpp:312–324,733–756` | Context-only Build cancellation vs immediate session CancelCopy on Remove/Close; provider faults/rooting remain explicit. |
| `ReplicaManager.cpp:792–870,1783–1832` | Remove retention/membership, sender close/copy cancel, write waiters before build cancel; exact rooted session prevents old-ID cleanup rebinding new Build. |
| `RemoteSession.cpp:252–365`; `ReliableOperationSender.cpp:113–137,548–755`; `AckSender.cpp:64–208` | Tokio deadlines/Notify drive real StartCopy, replication/context retry and ACK timers; no alternate sender or autonomous component task. |
| `ReplicaManager.Test.cpp:772–824` | Admission/helper ancestry; lifecycle/cancel/fault/timer vectors derive from production, not native test fixtures. |

### Secondary lifecycle — F04/F05/F06/F09/F11

Destination: [`secondary.rs`](../../../kuberic-runtime/src/replicator/v1/secondary.rs)
and adjacent tests; actual receiver/stream/establishment owners remain intact.

| Selected native material | Translation / exclusions |
|---|---|
| `SecondaryReplicator.cpp:393–550` | Admission retirement, ordinary Close/End/Abort, context sender/provider wait; caller futures/weak ingress replace COM/jobs, abandonment is not Close. |
| `SecondaryReplicator.CloseAsyncOperation.cpp:120–188,327–362` | Legacy ordinary dispatch abort/queue cleanup, reserved UpdateEpoch wait but not all ACK/context calls. Native TODO is not a universal graceful-drain guarantee. |
| `SecondaryReplicator.CloseAsyncOperation2.cpp:116–378` | EOS ordinary Close: incomplete-copy cleanup, EOS ACK, uninitialized abort bypass, copy then replication drain, context then epoch wait. Failed drain returns; Abort retains obligations. |
| `SecondaryReplicator.DrainQueueAsyncOperation.cpp:56–327`; `OperationStream.GetOperationAsyncOperation.cpp:139–184` | Actual dispatch then required-service-ACK drain. Empty-open is not EOF; stream fault preserves failed drain/EOS wait. |
| `SecondaryReplicator.cpp:1624–1965` | Reserve ACK epoch work before callback finish, call provider outside guards; full barrier failure differs from stream marker/provider fault. Retained real futures, no fabricated completion. |
| `SecondaryReplicator.cpp:862–914,1063–1123` | Provider acquisition/EndGetNext faults vs remote context errors; acquisition forces negative ACK only while copy pending. |
| `SecondaryReplicator.cpp:1967–2083,2235–2280`; `AckSender.cpp:64–208` | Actual copy/context checks, two-receiver ACK and stream fault/EOS waits; retained cleanup tokens avoid provider/root cycles. |

### Primary lifecycle and configuration — F02/F06/F07/F08/F09/F11

Destination: [`primary.rs`](../../../kuberic-runtime/src/replicator/v1/primary.rs),
[tests](../../../kuberic-runtime/src/replicator/v1/primary/tests.rs),
[configuration tests](../../../kuberic-runtime/src/replicator/v1/primary/tests/configuration.rs)
and existing manager/session/sender seams.

| Selected native material | Translation / exclusions |
|---|---|
| `PrimaryReplicator.cpp:261–315`; `PrimaryReplicator.CloseAsyncOperation.cpp:36–132` | Optional quorum prewait, timeout proceeds, manager/catchup cancellation and selected effect drain; Abort skips prewait. Rust activity/oneshot delivery is not client waiter execution. Creating-secondary branch excluded here. |
| `ReplicaManager.cpp:792–870,926–1007` | Cancel writes before session removal can invent quorum success, close senders/cancel builds; no arbitrary provider-call drain. |
| `PrimaryReplicator.cpp:550–728`; `ReplicaManager.cpp:2187–2222` | Bound endpoint/address lookup, idle context/copy ACK vs active replication ACK; CC/PC before idle lookup. |
| `RemoteSession.cpp:448–656` | Rooted/coalesced ordered sends/build gate/quorum/copy handoffs; all stages drain despite error/reentrancy and selected work can outlive sender Close. |
| `RemoteSession.cpp:98–112,252–365`; `ReliableOperationSender.cpp:548–755` | Build drivers then lasting primary timers; Notify registration before sampling, actual provider futures persist across interrupted polls. |
| `PrimaryReplicator.cpp:340–462` | Catch-up/current-only updates, fault rejection and outstanding catchup reevaluation; typed remote descriptors, not COM ABI. |
| `ReplicaManager.cpp:306–335,1205–1242` | ID reuse, session creation/Open/descriptor seed and replay from last apply+1; optional progress substitutes native unknown sentinel, reused identity/progress not overwritten. |
| `ReplicaManager.cpp:1244–1779` | CC/PC/idle reuse/quorums/captured marks/unknown-current/PC contraction/current replay/previous-only pause/retirement. Failure-atomic install and binding publication before reentrant replay. |
| `ReplicaManager.cpp:1738–1779`; `ReplicationSession.cpp:279–283` | Idle-to-active marking/average reset with actual sender/build roots, not synthetic copy success or secondary-to-primary role promotion. |

### Role, epoch and data-loss recovery — F01/F02/F03/F04/F09/F11

Destination: [`role.rs`](../../../kuberic-runtime/src/replicator/v1/role.rs),
[role tests](../../../kuberic-runtime/src/replicator/v1/role/tests.rs),
[data-loss tests](../../../kuberic-runtime/src/replicator/v1/role/tests/data_loss.rs)
and actual queue/receiver/primary/manager seams.

| Selected native material | Translation / exclusions |
|---|---|
| `ReplicatorState.cpp:39–126`; `Replicator.ChangeRoleAsyncOperation.cpp:37–514` | Edge matrix, provider/cache progress, close/create/handoff, idle-active drain, primary epoch and None; retained typed steps/futures replace COM roots. |
| `ReplicationQueueManager.cpp:59–160`; `SecondaryReplicationReceiver.cpp:61–155,689–706` | Actual queue/root/original-epoch transfer. Demotion trims oldest until native count/byte bound, requires committed tail and adopts service ACK mode; no reconstructed scalar history or synthetic ACK. |
| `SecondaryReplicator.CloseAsyncOperation.cpp:39–117,188–362`; `SecondaryReplicator.CloseAsyncOperation2.cpp:40–115`; `SecondaryReplicator.cpp:2110–2136` | Promotion vs ordinary Close, legacy required-ACK incomplete rejection, optimistic dispatch vs EOS data/ACK drain; actual context/provider futures retained. |
| `ReplicaManager.cpp:792–920`; `PrimaryReplicator.UpdateEpochAsyncOperation.cpp:31–90` | Demotion cancel before forced progress, issued-tail callback even duplicate epoch; timer driver active while provider pending. Old build providers remain for separate drain. |
| `SecondaryReplicator.UpdateEpochAsyncOperation.cpp:31–155`; `Replicator.UpdateEpochAsyncOperation.cpp:34–133` | Direct epoch serialization/minimum/duplicate/stale/discard/copy-fault and ACK drain. Direct secondary update is not provider call; stream barriers separate. Abort/interrupted work stays explicit. |
| `Replicator.OnDatalossAsyncOperation.cpp:30–157`; `ReplicatorState.cpp:129–139,177–186` | Fault preflight/CheckingDataLoss/provider callback/changed-only query/cache-before-reset/End restoration. Changed flag and original error retained privately even if query/reset fails. |
| `Replicator.cpp:659–708,799–838` | Temporary primary/ACK/context admission gating, existing activities rooted and timers driven without locks across callbacks. |
| `PrimaryReplicator.cpp:501–505`; `ReplicaManager.cpp:1015–1023`; `ReplicationQueueManager.cpp:148–160` | Real reset/next assignment/PC cursor reset. Pending work/selected activities/negative-overflow become explicit errors, not synthetic cancellation; configuration/session roots unchanged. |
| `Replicator.OnDatalossAsyncOperation.cpp:82–157` | Retain changed callback/query through Abort/interrupted polling; skip closed-primary reset, cannot publish recovery success. Abnormal End is Rust adaptation. |
| `PublicApi.Test.cpp:1349–1383`, **TestDataLoss(true/false)** | Changed/unchanged progress expectations; lower/zero/invalid, identity, retry, reentrancy/error/Abort cases derive additionally from production. |

### Pressure and slow-peer policy — F02/F03/F06/F07/F08/F11

Destinations: [`replica_manager/policy.rs`](../../../kuberic-runtime/src/replicator/v1/replica_manager/policy.rs),
[`primary/tests/policy.rs`](../../../kuberic-runtime/src/replicator/v1/primary/tests/policy.rs)
and actual primary/session/build/queue/carrier seams.

| Selected native material | Translation / exclusions |
|---|---|
| `ReplicaManager.cpp:530–545,716–778` | Freshness tick/strict age/committed retarget/current settings; caller-driven Tokio timer, not trace/monitoring implementation. |
| `ReplicaManager.cpp:1832–2076`; `CopySender.cpp:524–579`; `ReplicationSession.cpp:87–283` | Successful/full admission pressure, issued-tail retarget before disable/fault and utilization recheck; retain selected fault-state getters, sender Open and Close/timer cancellation as well as notification policy. Real completion-disable race, selected partial effects retained on later failure; Rust session/task/timer ownership replaces native timer/lock execution. |
| `ReplicaManager.cpp:173–284,350–432,1871–1980`; `standarddeviation.h:17–22`, `standarddeviation.cpp:32–76` | Spare quorum/age/fastest remote mean/deviation/strict outlier/least receive; preserve100ns threshold truncation without utility source. Fault excludes receive reclamation, not apply quorum/All/live MustCatchup. |
| `ReplicationSession.cpp:175–283`; `RemoteSession.cpp:448–490`; `ReplicaManager.cpp:1748–1750,2443–2450` | Sender close before reclaim, idle/active fault split and retrying notifications; sends do not ACK/clear retries. Faulted-idle promotion rejects. |
| `Replicator.cpp:946–980`; `OperationQueue.cpp:135–138,205–212`; `ComOperation.cpp:18–28` | LSN-span/logical-byte utilization with truncation; creation/transfer timestamps via Tokio; operation identity retained. |
| `src/prod/src/ServiceModel/reliability/replication/REConfigMacros.h:166–177`; `REInternalSettings.cpp:451–607` | Private settings/persisted mode sampling, not config-store or arbitrary public upgrade implementation. |
| `ReplicationSession.cpp:251–280`; existing logical `sf_v1.proto` | InduceFault with bound target, actual tonic notification; not native wire. Selected remote host authority remains Kuberic adaptation. |
| `DecayAverage.Test.cpp:65–107`; `ReplicatorSettings.Test.cpp:738–788` | Reviewed context only. Real sender latency observations test policy, not duplicated deviation oracle or whole native fixtures. |

### Ingress, Open and restart — F01/F04/F05/F07/F08/F09/F11

Destinations: [`ingress.rs`](../../../kuberic-runtime/src/replicator/v1/ingress.rs),
[`secondary/ingress.rs`](../../../kuberic-runtime/src/replicator/v1/secondary/ingress.rs),
role/transport seams, [ingress tests](../../../kuberic-runtime/src/replicator/v1/role/tests/ingress.rs)
and [Open tests](../../../kuberic-runtime/src/replicator/v1/role/tests/open.rs).

| Selected native material | Translation / exclusions |
|---|---|
| `Replicator.cpp:618–759`; `ReplicatorState.cpp:39–186`; `Replicator.ChangeRoleAsyncOperation.cpp:131–275,405–489` | Role/message/Closing/fault selection and actual owner admission windows; shared selection handle/outside-guard delivery replaces jobs/locks, not ACK. |
| `Replicator.CloseAsyncOperation.cpp:31–180`; `Replicator.cpp:659–708` | Direct Close/Closing/Closed with actual prewait/secondary Close; None remains separate. |
| `SecondaryReplicator.cpp:915–1035,1125–1263,1306–1396,1438–1621` | Fixed copy vs moving replication epoch, actual primary reply/current-primary ACK routes, batch decoding before admission. |
| `SecondaryReplicator.cpp:2282–2344`; `ReplicationTransport.cpp:684–832` | Target/initialized-copy checks and first fault; required external report callback, not automatic cleanup/membership removal. Reply routes outlive weak establishment ingress. |
| `src/prod/src/idl/public/FabricTypes.idl:46,779–781` | Normal/terminal metadata; atomic/non-normal rejected, not full metadata capability. |
| `Replicator.OpenAsyncOperation.cpp:29–81`; `ReplicatorState.cpp:16–37` | One Created Open, actual ingress/address without provider query/role. Performance monitor/native endpoint formatting excluded. |
| `ReplicationTransport.h:253–267,301–308`; `ReplicationTransport.cpp:872–878` | RAII logical registry and duplicate-identity error replace native demux root; dispatch unlocked, not authenticated production listener by itself. |
| `Replicator.cpp:296–367`; `Replicator.ChangeRoleAsyncOperation.cpp:131–208`; `ComProxyStateProvider.cpp:63–79` | Lazy normalized/cache progress/initial primary; retained futures replace synchronous calls. Successful negative→0, errors retained; query lane serialization is Rust policy, not full native query concurrency. |
| `Replicator.CloseAsyncOperation.cpp:31–100,131–180`; `Replicator.cpp:178–253`; `ReplicatorState.cpp:177–242` | Registration through Close prewait, release even on error; Abort/RAII seal but keep provider obligations, not invented drain. |
| `ReplicationQueueManager.cpp:29–71`; `ReplicaManager.cpp:1205–1779`; `PrimaryReplicator.BuildIdleAsyncOperation.cpp` | Empty process queue from provider progress, missing-replay error then real caller-directed copy/build. No reconstructed byte suffix or imported placement/rebuild controller. |

### Service adapter and protected facade — F01/F02/F04/F05/F09/F11

Destinations: [`service_adapter.rs`](../../../kuberic-runtime/src/replicator/v1/service_adapter.rs),
[adapter tests](../../../kuberic-runtime/src/replicator/v1/service_adapter/tests.rs),
[`service_adapter/facade.rs`](../../../kuberic-runtime/src/replicator/v1/service_adapter/facade.rs),
[facade tests](../../../kuberic-runtime/src/replicator/v1/service_adapter/tests/facade.rs)
and query/Build End seams.

| Selected native material | Translation / exclusions |
|---|---|
| `ComReplicator.cpp:219–313`; `ComReplicator.ReplicateOperation.cpp:23–74`; `PrimaryReplicator.ReplicateAsyncOperation.cpp:31–66` | Weak service handle returns actual assignment separately from retained quorum future; waiter drop is not rollback or service commit. |
| `Replicator.ReplicateAsyncOperation.cpp:31–98`; `Replicator.cpp:889–907`; `EnumOperators.cpp:85–129` | Lossless buffer validation then access then admission; empty data remains valid despite misleading native comment. Host supplies selected access policy. |
| `Replicator.cpp:296–399`; `PrimaryReplicator.cpp:319–336`; `SecondaryReplicator.cpp:553–619` | Opened normalized provider/cache/capability0, native primary/secondary getters and incomplete-copy fault; no provider-durability reinterpretation. |
| `Replicator.cpp:407–562`; existing Open/role/epoch/data-loss/Close/Abort sources | Resumable actual controls/exclusive lane/projected weak handles/driver; not full concurrent facade interleavings. |
| `ComOperationStream.GetOperationOperation.cpp:23–72`; `ComFromBytesOperation.cpp:139–257`; existing stream/epoch sources | Shared queues/internal provider barriers/parameterless ACK, not second ACK machine/rewind/fake EOF/negative item ACK/automatic apply. |
| Existing BuildIdle/primary Close sources | Separate outside-guard End Notify/liveness/drop observation, cancellable/multiple observers; waiter drop vs cancellation/removal/drain remain distinct. |
| `Replicator.cpp:296–562`; `ReplicatorState.cpp:39–126` | Protected trait task uses bounded request channel; exact public descriptions bind private snapshots, not fabricated identity conversion. |
| `Replicator.CatchupReplicaSetAsyncOperation.cpp:29–94`; existing primary/BuildIdle lifetimes | Driver-rooted build/catchup and intact retired-primary transfer; persistent futures retain wake registrations, not fabricated completion. |
| `Replicator.cpp:178–253`; `ReplicatorState.cpp:177–248`; `Replicator.CloseAsyncOperation.cpp:31–154` | Synchronous Abort admission/revocation then retained task coordination/provider drain, unlocked callbacks. Ordinary Close vs separate cancellation-safe join/abnormal Drop; no full native interleaving parity. |

Protected `Result<bool>` cannot carry changed flag alongside failed End; richer
private recovery outcome remains private. Tests use actual compiled owners and
helper-free provider callbacks, not native fixture/binary execution.

### Host access and outer lifecycle — F01/F09/F11

Private destination: [`kuberic-runtime/src/host/v1_lifecycle.rs`](../../../kuberic-runtime/src/host/v1_lifecycle.rs)
and [host adapter tests](../../../kuberic-runtime/src/replicator/v1/service_adapter/tests/host.rs).
This native-shaped private host is distinct from selected
`host/hosting.rs`/`selected_access.rs`/`public_lifecycle_program.rs`.
Selected [policy:99–184][selected-policy], [activation:590–629][selected-activation],
[Close:550–638][selected-close] and [recovery:79–115][selected-recovery] port the
bounded reachable behavior below using the production journal and ownership
mechanisms; ancestry alone is not the production evidence.

| Selected native material | Translation / exclusions |
|---|---|
| `RA/FailoverUnitProxy.ReadWriteStatusCalculator.cpp:34–381` | Native reachable stage selector/table/precedence; TryAgain→ReconfigurationPending. Demotion after catch-up selects TransitioningRole/Completed, not table CatchupCompleted; invalid inputs explicitly error. |
| `RA/FailoverUnitProxy.cpp:1399–1455` | Local/remote Up/Ready CC minimum/majority and optional PC majority; exact bindings replace configuration-map identity. Host availability is not ACK liveness/W/provider durability. |
| `RA/FailoverUnitProxy.ChangeReplicatorRoleAsyncOperation.cpp:99–140`; `RA/FailoverUnitProxy.ChangeReplicaRoleAsyncOperation.cpp:93–150` | Actual child callbacks advance distinct observations/service address; nonprimary clears membership. Caller-retained futures/activation seam, not RA program. |
| `RA/FailoverUnitProxy.CloseAsyncOperation.cpp:30–110,164–276` | Access revocation, role mismatch abort, replicator then app Close, failed-child abort with diagnostics and native outer completion; repeated Close avoids callbacks. Retained real app future/explicit join/Abort, not fabricated successful child drain. |

Host owns access/proxy state outside engine; opaque probes and neutral callbacks
avoid concrete core→host coupling. Already-open application, private availability
snapshots and activation are explicit seams, not production registry/failure
detector/RA/FM/placement. Shared handle access rebinding runs outside guards;
optional provider evidence does not redefine native access. Private host
in-memory visibility evidence and selected real-file service evidence differ.

Selected deterministic [stage tests:133–1025][selected-stage-tests] use actual
public hosts, committed service reads and raw write assignment. They include
split/recipe promotion, joint/current-only configuration, missing C/P,
pre/post-Revoke demotion, final-wait-before-receipt denial, cancellation/exact
replay and stale owner/final release. [Close tests:3114–3505][selected-close-tests]
separate child errors, outer success, interrupted callbacks and real retained
provider joins; some recovery cuts are deliberately seeded. These Rust runs
are not native executable/differential or exhaustive crash-cut qualification.
Local abort-both/skip-later-graceful containment intentionally differs from
native's per-child Abort/continued service Close. Required protocol-13
availability facts are controller observations, not a new failure detector;
old descriptor-bearing state is rejected rather than defaulted available.

### Service-owned KV and SQLite commitment — F02/F04/F05/F07/F09/F11

Destinations:
[`examples/kvstore2/src/v1.rs`](../../../examples/kvstore2/src/v1.rs),
[`persistence.rs`](../../../examples/kvstore2/src/persistence.rs),
[KV adapter tests](../../../kuberic-runtime/src/replicator/v1/service_adapter/tests/kv.rs),
[`examples/sqlite/src/v1.rs`](../../../examples/sqlite/src/v1.rs),
[SQLite adapter tests](../../../kuberic-runtime/src/replicator/v1/service_adapter/tests/sqlite.rs).
Production services select these companions; repository-only dev/testing bridges
also compose private owners without exposing engine types.

| Selected native evidence | Service adaptation / exclusions |
|---|---|
| `src/prod/src/Store/ReplicatedStore.TransactionReplicator.cpp:606–796,848–900` | Source-guided service-owned ordering, not whole KVS or an SF SQLite port. Retained Rust FIFO worker/task/channel/oneshot replaces transaction/COM roots; assignment→quorum→local commit→visibility/client success remains separate. KVS implicit completion/batching/tombstones/local-store internals excluded. |
| Existing F02 assignment/completion and F11 lifetimes | Actual LSN/quorum future; KV durable prepare/fsync/rename; SQLite blocking SQL/real VFS waits quorum, commits frames then publishes WAL/confirms SQL before response. Cancellation does not retract work/release serialization; post-admission failure ambiguous. C-ABI callback panic becomes fault, not fallback. |
| Existing F04/F05 receiver/stream/ACK/EOS sources | Real persistence/snapshot install/validation before ACK; no item-level negative ACK. EOS-enabled fault/Abort separate from legacy unsupported behavior; versioned snapshot content belongs to service. |
| Existing F07 enumeration/barrier and F09 role/recovery sources | Snapshots wait local commitment; new-instance recovery discards uncommitted prepares/validates local state. Real-file restart/copy/queue transfer and inherited LSN, not same-lifetime rollback/native queue reconstruction/old-store migration. |

KV shares existing mutation/snapshot/file primitives; unreadable state returns
IO error rather than empty fallback. SQLite shares WAL/VFS/frame-log/versioned
geometry and adds page-size/quick-check to snapshot validation. Actual file IO
and deterministic gates are not fsync instrumentation, kernel power-loss
qualification, native binary or hosted differential evidence.

## Native exposure and settings targets

This is retained **target ancestry**, not implemented public Kuberic settings.
Broader P7 remains deferred inside the original target. Current selected
address/persistence/bearer/basic-report boundaries and fixed policies are in
[current semantics](classic-v1-runtime.md#production-policies-and-security).

| Native source at selected pin | Target / classification retained |
|---|---|
| `ComReplicator.h:12–35`; `ComReplicator.cpp:53–83,490–808,1002–1118` | Actual QI includes settings2/specific quorum; settings getter merges effective address/security. Public update compares breaking fields; credential changes validated/updated, not ignored. Internal reservation/batch E_NOTIMPL, queue counters distinct; managed Buffer/Count validation does not establish Offset bounds checking here. |
| `ComOperationStream.h:12–24`; `ComOperationStream.cpp:48–78`; `OperationStream.cpp:66–85` | stream2 exposed even when fault/EOS disabled; internal batch E_NOTIMPL; invalid fault kind before policy, disabled→InvalidOperation. |
| `reconfig.h:12–17`; `src/prod/src/ServiceModel/reliability/replication/REConfigMacros.h:136–207`; `src/prod/src/idl/public/FabricTypes.idl:2910–2960` | Public base/EX1–EX4 fields/defaults vs internal policy. Legacy flags populate both roles, EX3 overrides per role before comparison; config-store dynamic updates are not arbitrary public upgrades. |
| `ComReplicator.cpp:490–532,550–582,639–808`; `REConfigMacros.h:136–207` (full path above) | Retry5000ms, ACK15ms, requireACK false; native public primary64/1024/memory0, secondary64/2048/memory0, copy64/1024/no maximum-memory; max message52,428,800, clear-ACKed false, pending-Close0, max-pendingACK32, localhost:0. EOS/fault public false vs internal true. These are **native targets**, not selected production defaults. |
| Same settings sources | Legacy initial128→both128, EX3 primary64→64/128; memory600 without legacy max-count flag→counts0/0, with max1024→1024/1024 before EX3/validation. Specified retry6000 rejects, equal/unspecified is not that error. Listen/publish fields not compared as breaking; native credential updates separate. |
| `ReplicaManager.cpp:1092–1099`; `REConfigMacros.h:165–177` (full path above) | Specific quorum compares live marked minimum to current Q; AllowMultipleQuorumSet true, idle freshness60s are internal policy, not arbitrary public settings. |
| `OperationStream.GetOperationAsyncOperation.cpp:116–136`; `SecondaryReplicator.cpp:165–193` | Ordinary null EOF vs enabled explicit EOS ACK; persistence forces explicit ACK even if require=false. |
| `RA/ComStatefulServicePartition.cpp:208–350`; `BatchedHealthReporter.cpp:62–125` | Fault/load/move/scoped health acceptance is not immediate platform convergence; batching/clear are separate targets. |
| `src/prod/src/ServiceGroup/Service/ServiceGroup.StatefulService.h:153–180`; `src/prod/src/ServiceGroup/Service/ServiceGroup.StatefulServiceUnit.h:27–58` | Separate atomic wrapper boundary, absent from ordinary ComReplicator; no ServiceGroup port. |
| `ComProxyStateProvider.cpp:19–26`; `src/prod/src/data/txnreplicator/ComStateProvider.h:10–29` | Distinct internal V2 copy-until-latest marker, not public specific-quorum or classic Build; no V2 implementation incorporated. |

## Selected acceptance and test ancestry

IDs retain source selection, not a second executable scenario catalog.
**D** = selected native assertions simplified/transformed; **S** = production
source-derived vectors. Current support/limits are in the ledger's
[acceptance mapping](sf-v1-port-ledger.md#original-target-and-acceptance-family-disposition).

| Material / IDs | Exact native test/range/case or production ancestry | Transformation |
|---|---|---|
| A1 D/S | `PublicApi.Test.cpp:454–504` **BeginReplicateReturnsSOKIfAsyncOperationFails**; `ReplicationQueueManager.cpp:369–408`, `ReplicaManager.cpp:349–390` | Begin S_OK/End NotPrimary split; progress10/11/full12 are source-derived, not native constants. |
| Q1 D/S | `OperationQueue.Test.cpp:1049–1095` **TestOutOfOrderOperations1/2**; `OperationQueue.cpp:677–737,990–1047` | Fixed receive2,1/ACK2,1 reduced from randomized1024-item permutations, no random/Boost fixture. |
| Q2 D/S | `OperationQueue.Test.cpp:435–461` **TestEnqueueOperationsWithMemoryLimit**; `OperationQueue.cpp:256–318` | Selected4×100/retained100/reject301/accept100; production neighbor add300 evicts completed100 to total600, sufficient count growth/no pending drop. |
| Q3 D/S | `OperationQueue.Test.cpp:607–615` **TestCompleteBeforeCommit**; `OperationQueue.h:76–87` | False/true assertions; no-history capability100→0 source-derived separately. |
| Q4 D/S | `OperationQueue.Test.cpp:867–901` **TestSecondaryQueueTrim**; `OperationQueue.cpp:371–399` | Selected heads/history and LSN duplicate accounting, not ring allocator port. |
| M1 D; M2–M5 S | `ReplicaManager.Test.cpp:154–220` named quorum cases above; `PrimaryReplicator.CatchupAsyncOperation.cpp:43–104`; `ReplicationQueueManager.cpp:261–272,319–357`; `ReplicaManager.cpp:1278–1333,2328–2362,2471–2524` | Quorum/reclaim, moving All, captured B/current Q/live M, W1 asymmetry and catch-up caps; actual owner arithmetic, not duplicate executable oracle. |
| S1–S3 S | `SecondaryReplicator.cpp:165–193`; `SecondaryReplicationReceiver.cpp:177–202,632–740`; `SecondaryCopyReceiver.cpp:24–80`; `ComFromBytesOperation.cpp:167–224` | Mode split/contiguous receive vs completion/shared terminal lifetime/late callback, not identical native error-result surface. |
| R1 D; R2–R3 S | `ReliableOperationSender.Test.cpp:102–143` **TestReceiveACKProgress**; `ReliableOperationSender.cpp:548–579,636–679,732–755`; `RemoteSession.cpp:585–611`; `AckSender.cpp:64–127,174–208` | Pending-list/receive-vs-apply/timer/window/RequestAck/order/final flush, deterministic time replaces native sleeps. |
| C1/C6 S | Actual QI/settings targets immediately above; `ComReplicator.cpp:1002–1050`; `OperationStream.cpp:66–85` | Actual exposed/internal/unsupported distinction, not every SDK declaration or implemented public Kuberic capability. |
| C2 S | `ComProxyAsyncEnumOperation.cpp:108–121,182–195`; `CopyAsyncOperation.cpp:171–254`; `SecondaryReplicator.cpp:982–1003` | Nonnull empty item vs null EOF/terminal enumeration; persisted target context, volatile omission. |
| C3 D/S | `CopyAsyncOperation.Test.cpp:418–449` **CopyAsync_QueueFull_Cancelled**; `CopyAsyncOperation.cpp:42–49,242–254`; `SecondaryCopyReceiver.cpp:388–395` | Native15/15/4/cancel-at12 reduced full/parked/cancel vector, no complete replay fixture. |
| C4–C5 S | `PrimaryReplicator.BuildIdleAsyncOperation.cpp:111–144`; `CopySender.cpp:524–579,555–579`; `ReplicaManager.cpp:745–774,1983–2022` | EOF-issued target/copy+replication gates/upward pending retarget; not snapshot-only Build success. |
| T1 D/S; T2 S | `ReplicationTransport.Test.cpp:594–617` **TestDropMessageToNotMatch**; `PrimaryReplicator.cpp:632–683`; `ReplicationSession.cpp:217–243,364–384,423–463`; `RemoteSession.cpp:757–790` | Remove sockets/Sleep(100), retain route/incarnation/role/epoch/expiry/failure obligations; accepted send≠ACK. |
| D1 S | `src/prod/src/idl/public/FabricRuntime.idl:636–683`; `ReplicationOperationHeader.h:25–42`; `ReplicationAckMessageBody.h:17–24` | Empty-array/empty-segment boundaries and two epochs/sequence spaces inform private lossless codec; not native serialization/public segmented policy parity. |
| L1–L3 S; E1–E2 S | `ReplicatorState.cpp:39–126`; `ReplicationQueueManager.cpp:58–160`; `Replicator.OnDatalossAsyncOperation.cpp:82–126`; `Replicator.CloseAsyncOperation.cpp:134–192`; `RA/FailoverUnitProxy.CloseAsyncOperation.cpp:209–276`; `ReplicaManager.cpp:871–920`; `PrimaryReplicator.UpdateEpochAsyncOperation.cpp:31–90`; `SecondaryReplicator.cpp:951–1003` | Role edges/actual queues/reset, direct vs outer Close, issued epoch and fixed copy epoch. Selected outer success retains child diagnostics and actual cleanup; inner containment action order remains explicitly local. |
| H1–H2 S; H3 D/S | `RA/FailoverUnitProxy.ReadWriteStatusCalculator.cpp:34–145,288–381`; `BatchedHealthReporter.Test.cpp:107–135` **TestWarningsWithinIntervalReport**; `BatchedHealthReporter.cpp:81–125` | Reachable stage selectors, not every table row; selected bounded stage evidence above. Temp2/latest-warning/Close-clear expectations use named timer input, not sleep/platform convergence; broader reporting deferred. |
| Protected resolved fixtures | Kuberic `application.rs:31–37`, protected traits and external positive/privacy fixture infrastructure | Kuberic-authored compiler identities/trait implementations, not native COM or segmented codec. |

### Reviewed-only context

These were source/context selections, not whole-test translations or executions:

- Native behavior comparisons (not additional incorporated implementations):

  | Native context | Purpose / boundary |
  |---|---|
  | `RA/FailoverUnitProxy.cpp:428–461,1376–1396`; `RA/ComStatefulServicePartition.cpp:75–147,482–486` | Direct Abort/access calculation and cached public status, not per-transaction service isolation. |
  | `RA/ProxyActionsList.cpp:63–73,94–148`; `RA/FailoverUnitProxy.ReplicatorCatchupReplicaSetAsyncOperation.cpp:107–118` | Distinct action-list/direct Abort, normal child role order and catch-up completion/error scope; not an RA orchestration port. |
  | `ReplicaManager.cpp:1025–1034,1671–1785,2467–2528`; `PrimaryReplicator.CatchupAsyncOperation.cpp:42–104`; `SecondaryCopyReceiver.cpp:160–287,377–427`; `Replicator.ReplicateAsyncOperation.cpp:43–64,103–108` | Native progress/configuration/copy/admission/completion comparison, not the removed engine's receipt or application-durability contract. |
  | `src/prod/test/FabricTest/TestStoreService.cpp:108–155,791–817,866–883` | One native test service's staged visibility/EndReplicate/read-status example, not generic application isolation or an imported fixture. |
  | `src/prod/src/data/txnreplicator/ComTransactionalReplicator.cpp:508–512`; `src/prod/src/data/txnreplicator/loggingreplicator/LoggingReplicatorImpl.cpp:435–445` | Distinct transactional-layer interface/query context, not incorporated V2 implementation or classic queue semantics. |
  | `ReplicatorState.cpp:28–248`; `Replicator.CloseAsyncOperation.cpp:32–200`; `RA/FailoverUnitProxy.CloseAsyncOperation.cpp:29–109`; `RA/ComStatefulServicePartition.cpp:164–350` | Facade lifecycle, direct/outer Close and partition report target context; no claim the selected public host reproduces every branch. |
  | `ReplicaManager.cpp:349–454,1061–1104,1783–1833,2328–2528`; `ReplicaManager.h:285–349`; `ReplicationQueueManager.cpp:369–411`; `PrimaryReplicator.CatchupAsyncOperation.cpp:62–106` | Additional configuration/quorum/admission/catch-up reference boundaries, not new algorithm-domain qualification. |
  | `RemoteSession.cpp:492–670`; `RemoteSession.h:202–259`; `ComFromBytesOperation.cpp:167–262`; `SecondaryReplicator.UpdateEpochAsyncOperation.cpp:31–156` | ACK/callback/provider-boundary context from the public contract comparison; distinct from a native test or whole-owner translation. |
  | `src/prod/src/managed/Api/src/System/Fabric/IStateReplicator.cs:19–81,102–127`; `src/prod/src/managed/Api/src/System/Fabric/IStatefulServicePartition.cs:16–45` | Managed API contract context, not imported managed implementation or a substitute for actual native facade exposure. |

[selected-policy]: https://github.com/youyuanwu/kuberic/blob/aad23335c219a576d69133cee883bbd63e40b68a/kuberic-runtime/src/host/selected_access.rs#L99-L184
[selected-activation]: https://github.com/youyuanwu/kuberic/blob/aad23335c219a576d69133cee883bbd63e40b68a/kuberic-runtime/src/host/selected_access.rs#L590-L629
[selected-close]: https://github.com/youyuanwu/kuberic/blob/aad23335c219a576d69133cee883bbd63e40b68a/kuberic-runtime/src/host/public_lifecycle_program.rs#L550-L638
[selected-recovery]: https://github.com/youyuanwu/kuberic/blob/aad23335c219a576d69133cee883bbd63e40b68a/kuberic-runtime/src/host/public_lifecycle_terminal.rs#L79-L115
[selected-stage-tests]: https://github.com/youyuanwu/kuberic/blob/aad23335c219a576d69133cee883bbd63e40b68a/kuberic-runtime/tests/support/staged_access.rs#L133-L1025
[selected-close-tests]: https://github.com/youyuanwu/kuberic/blob/aad23335c219a576d69133cee883bbd63e40b68a/kuberic-runtime/tests/public_v1_recovery.rs#L3114-L3505

- `ReplicationSecondary.Test.cpp:529–579,1450–1507,1550–1587` — progress/drain modes.
- `ChangeRole.Test.cpp:25–45` — registrations, not copied network run.
- `ReplicateCopyOperations.Test.cpp:708–741,1093–1105,1455–1467` — empty context/EOS.
- `ReplicatorSettings.Test.cpp:706–795` — config-store notifications, not arbitrary
  public update success.
- `src/prod/src/Reliability/Replication/test/CMakeLists.txt:8–30,37–64` — dependency
  targets only; no native build graph imported.

## Execution and platform substitutions

Rust ownership, retained futures/tasks, weak handles, outside-guard effects,
Notify/wakers, injected Tokio time, bounded request/carrier queues and tonic
protobuf replace COM Begin/End, Threadpool/job queues, ReaderQueue and platform
transport/timer/root plumbing. They must retain algorithm-affecting order,
retry, epoch, ACK, error and lifetime distinctions; they are not a claim of
every native concurrent interleaving or HRESULT/COM/wire result.

The selected host/carrier (`service_adapter/production.rs`,
`production_transport.rs`, `host/native_transport.rs`) composes the registered
owners with bearer/exact journal authority. Controller PC/CC staging,
copy-before-vote and frozen process/storage/history bindings are Kuberic
adaptations, not an RA/FM port. KV/SQLite persistence is service-owned, not an
engine durable-progress prerequisite. Source/test evidence classes and current
limits are [explicit in the ledger](sf-v1-port-ledger.md).

## Third-party boundary and future additions

Native
[`ThirdPartyNotices.txt`](https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/ThirdPartyNotices.txt)
is **heterogeneous**, not an all-MIT manifest: jQuery/Sizzle, moby-derived
Apache-2.0 and Boost.Asio notices have their own origins/licenses.
Root MIT alone does not license every vendored file.

Selected Microsoft production/test assertion material was reviewed for its own
headers/origin. No Boost harness, Common/Transport/KTL utility implementation,
third-party asset or native dependency build graph is incorporated by these
translations. Upstream test linkage does not imply those utilities were ported.
Future selections must extend this register with immutable pin, path/range/case,
Rust destination, transformation, retained notice, third-party origin and actual
execution/review status. Do not omit applicable notices, blindly copy unrelated
umbrella notices, or infer all-distribution legal certification from this register.
