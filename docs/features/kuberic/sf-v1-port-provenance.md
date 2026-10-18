# SF V1 Port Provenance and Notices

## Selected upstream and license

The P1 [port ledger](sf-v1-port-ledger.md), including its documented acceptance
scenarios and selected native test expectations, derives from Microsoft Service
Fabric at immutable commit
[`3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d`](https://github.com/microsoft/service-fabric/tree/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d).
The authorized reference checkout was reviewed read-only and clean at that pin.
No native binary or native test was run. P1 incorporated no implementation;
the queue and bounded sender/session/ACK slices below now translate selected native production
algorithms and test assertions into Rust. No native transport library or test
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

**Not implemented:** carrier/transport error-to-fault mapping, demux, buffer
adapter, manager/receiver/copy/provider/role owners; ReplicationSession
slow-fault flags/notification timer, MustCatchup and promotion-owner mechanics.
An epoch/progress view or handoff is not build/quorum/copy completion. Existing
protected aliases/interfaces and selected production factory remain unchanged.
No new dependency, native harness, Common/Transport/KTL/Boost implementation or
dependency graph is imported. Full P2 and activation/hosted/native/differential
parity remain unestablished.

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
