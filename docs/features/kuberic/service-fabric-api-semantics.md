# Service Fabric Stateful API Semantics and Kuberic Conformance

> Historical pre-cutover contract and audit context. The engine-owned commit,
> receipt, ordered progress-pair and credential-free transport statements below
> are superseded by [Classic V1 Runtime](classic-v1-runtime.md). Preserve their
> source/audit context; they do not describe the selected implementation.

## Scope

This document summarizes the Service Fabric V1 semantics implemented by
Kuberic's Public V1 runtime. It does not define the Service Fabric V2
Transactional Replicator.

**Current behavior versus target:** this page records the current Kuberic
contract, including its qualified native differences; it is not a full native
conformance claim. The [faithful SF V1 Rust async/tonic port plan](sf-v1-semantic-alignment-plan.md)
defines the future source-guided target and dependency order. Stronger current
durability/access rules are not automatically SF-equivalent, and historical fix
status does not exempt those rules from reassessment.

## Ownership

| Owner | Responsibility |
|---|---|
| Controller/local agent | Replica identity, role intent, epoch, PC/CC configuration, lifecycle sequencing, and access decisions |
| Stateful service/provider | Application state, applied/committed progress, copy representation, retained operations, and data-loss correction |
| V1 Replicator | Live role engine, queues, transport, peers, ACK aggregation, quorum completion, copy, and replication |

The V1 Replicator is restart-reconstructable and has no independent
continuation log.

## Lifecycle Ordering

Ordering is part of the contract:

1. Open the application and coherent Replicator bundle.
2. Keep access closed while validating identity, history, provider state, and
   accepted lifecycle authority.
3. Apply role/epoch/configuration operations for the exact authority.
4. Complete required catch-up or build work.
5. Grant access only after public completion and provider/writer fencing.
6. Revoke access and cancel authority-scoped work before supersession,
   retirement, Close, or Abort.

The same calls in a different order are not equivalent.

Access denial is not a role change. The host's access fence and durable Revoke
instruction do not call application `change_role(None)`. During a swap the
source retains its replication role and admitted authority through final
catch-up; only afterward does its actual replicator role change precede the
matching application role change. Both standalone and controller-composed
swap recipes retain that ordering
([swap sequence:19-39](../../../kuberic-runtime/src/host/public_lifecycle_program.rs#L19-L39);
[real roles:267-298](../../../kuberic-runtime/src/host/public_lifecycle_program.rs#L267-L298);
[Revoke:482](../../../kuberic-runtime/src/host/public_lifecycle_program.rs#L482);
[host fence:617-624](../../../kuberic-runtime/src/host/hosting.rs#L617-L624)).
Existing completion, coherent authority, and applied/committed progress gates
still govern access reopening; a role callback is not an access grant.

## Read Visibility and Admission

The built-in runtime conservatively denies **new normal reads** before primary
provider apply can expose a mutation. It publishes ambiguity with existing
applied/committed counters, then revalidates authority before entering the
provider. The abandonment guard cannot claim the allocated LSN before that
entry. Secondary ingress already publishes the same conservative fence before
service delivery, without crediting unacknowledged persistence
([primary:3076-3092](https://github.com/youyuanwu/kuberic/blob/66ad81d139453047f4df2e9282d7b34c79ff7cd9/kuberic-runtime/src/replicator/stateless/mod.rs#L3076-L3092);
[secondary:1186-1205](https://github.com/youyuanwu/kuberic/blob/66ad81d139453047f4df2e9282d7b34c79ff7cd9/kuberic-runtime/src/replicator/stateless/mod.rs#L1186-L1205)).

Provider return, gate release, equal raw counters or caller cancellation is not
quorum commitment. Ambiguous/error/dropped work stays fenced until verified
current-authority settlement; late progress cannot clear a separate
authority/fault fence. Genuine commitment and compatible public projection
refresh restore Primary read/write and ActiveSecondary read-only admission
([host publication:957-987](../../../kuberic-runtime/src/host/hosting.rs#L957-L987);
[projection readiness:545-605](../../../kuberic-runtime/src/host/hosting.rs#L545-L605)).

This is **Kuberic's conservative admission policy**, not identical SF access
timing or application transaction isolation. Normal
[`KvService::get`:68-77](../../../examples/kvstore2/src/service.rs#L68-L77)
checks status and then separately reads the applied map. A read admitted
before a later fence, raw provider access, or a custom application that bypasses
admission needs service-owned isolation. Polling partition status does not
universally contain application views. Kuberic's readable ActiveSecondary
policy also differs from the inspected classic SF partition path below.
SF-01's [exact regressions and closure](service-fabric-contract-audit.md#sf-01-applied-kv-state-is-visible-before-the-read-fence)
establish admission separately from the
[SF-D02 progress-query closure](service-fabric-contract-audit.md#sf-d02-primary-progress-promise-exceeds-the-built-in-query-result).

## Role-Specific Progress and Recovery

The built-in scalar `Replicator::current_progress` uses the **internal
replicator role**, not the host's serving-role projection or access status
([query:2765-2775](https://github.com/youyuanwu/kuberic/blob/66ad81d139453047f4df2e9282d7b34c79ff7cd9/kuberic-runtime/src/replicator/stateless/mod.rs#L2765-L2775);
[contract:45-58](../../../kuberic-runtime/src/replicator/traits.rs#L45-L58)):

| Internal state | Successful scalar result | Meaning |
|---|---|---|
| Primary, including access-closed writes, promotion and data-loss recovery | Acknowledged locally durable committed boundary | Not a locally applied tail awaiting quorum; not an access grant |
| Active/idle secondary | Highest contiguous durably accepted boundary | May exceed commitment; does not certify readable data |
| Opened role None | Validated durable retained applied tail, including a suffix | Election-only recovery observation with read/write access closed |
| Closed | `Closed` error | No progress observation |

Primary local apply/publication precedes peer quorum; the acknowledged committed
cache advances only after applicable PC/CC quorum, local durable commit, progress
validation and exact-authority revalidation
([write:3103-3195](https://github.com/youyuanwu/kuberic/blob/66ad81d139453047f4df2e9282d7b34c79ff7cd9/kuberic-runtime/src/replicator/stateless/mod.rs#L3103-L3195)).
Cancellation or commit error can leave a retained applied suffix without advancing
the scalar. Conversely a failed caller can have a real durable committed effect:
drained epoch/configuration reconciliation validates and adopts provider commitment
([reconcile:1012-1030](https://github.com/youyuanwu/kuberic/blob/66ad81d139453047f4df2e9282d7b34c79ff7cd9/kuberic-runtime/src/replicator/stateless/mod.rs#L1012-L1030)).
The query is conservative acknowledged engine progress, not a fresh read of
arbitrarily changed provider counters.

**Host reports are a separate two-boundary observation**, not two scalar calls.
Built-in reporting retains `first=committed,last=applied/accepted`; internal
Primary reporting reads provider commitment while secondary reporting uses
service-acknowledged counters
([report pair:130-155](https://github.com/youyuanwu/kuberic/blob/66ad81d139453047f4df2e9282d7b34c79ff7cd9/kuberic-runtime/src/replicator/stateless/mod.rs#L130-L155);
[public report:669-712](../../../kuberic-runtime/src/host/hosting.rs#L669-L712)).
An access-fenced internal Primary may therefore report Recovery with a retained
tail larger than its scalar. Election/configuration use these reports, preserving
the tail; they do not infer serving permission from it.

Open validates the retained suffix. Role change does not commit or truncate it;
catch-up settlement uses the separate cached pair and exact retained operations
([open:2532-2564](https://github.com/youyuanwu/kuberic/blob/66ad81d139453047f4df2e9282d7b34c79ff7cd9/kuberic-runtime/src/replicator/stateless/mod.rs#L2532-L2564);
[role:2624-2675](https://github.com/youyuanwu/kuberic/blob/66ad81d139453047f4df2e9282d7b34c79ff7cd9/kuberic-runtime/src/replicator/stateless/mod.rs#L2624-L2675);
[settlement:1562-1675](https://github.com/youyuanwu/kuberic/blob/66ad81d139453047f4df2e9282d7b34c79ff7cd9/kuberic-runtime/src/replicator/stateless/mod.rs#L1562-L1675)).
Thus retained `(applied=11,committed=10)` reports scalar 11 in opened None or
secondary, 10 after internal promotion, and 11 only after successful settlement.
The typed data-loss recipe samples progress after becoming internally Primary
but before catch-up/admission. Its persisted Recovery scalar snapshot `(10,10)`
is not proof of retained-tail equality; the independent report remains `(10,11)`
until settlement
([recipe:134-166](../../../kuberic-runtime/src/host/public_lifecycle.rs#L134-L166);
[observation:1829-1839](../../../kuberic-runtime/src/host/sqlite_store.rs#L1829-L1839);
[nonempty regression:4990-5054](../../../kuberic-runtime/tests/public_v1_recovery.rs#L4990-L5054)).
Committed-watermark propagation and coherent read/write admission remain
independent requirements. SF-D01's catch-up-capability mismatch is unchanged.

### Native SF Progress Is Role-Specific, Not a Universal Durability Promise

The read-only local SF checkout was reverified clean at
`3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d` for SF-D02.
Paths below are under `src/prod/src/` at that pin:

- Native V1 dispatches primary, secondary and initially opened queries separately
  ([Replication/Replicator.cpp:296-358](https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Replication/Replicator.cpp#L296-L358)).
  Primary delegates through
  [PrimaryReplicator.cpp:319-326](https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Replication/PrimaryReplicator.cpp#L319-L326)
  and [ReplicaManager.cpp:1025-1034](https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Replication/ReplicaManager.cpp#L1025-L1034)
  to queue commitment, normally derived from apply ACK quorums
  ([ReplicaManager.cpp:2467-2528](https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Replication/ReplicaManager.cpp#L2467-L2528)).
- Persisted SF secondary progress is contiguous **received** progress and can
  precede service ACK; it is not Kuberic's stronger durably accepted boundary
  ([SecondaryReplicationReceiver.cpp:177-192](https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Replication/SecondaryReplicationReceiver.cpp#L177-L192)).
- Initial primary progress inherits provider recovery progress. Promotion drains
  the secondary and transfers its queue; the new queue treats its retained prefix
  as committed before subsequent reconfiguration catch-up
  ([ChangeRole:130-194,369-485](https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Replication/Replicator.ChangeRoleAsyncOperation.cpp#L130-L485);
  [ReplicationQueueManager.cpp:29-91](https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Replication/ReplicationQueueManager.cpp#L29-L91)).
  Data-loss reset re-reads provider progress
  ([OnDataloss:82-126](https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Replication/Replicator.OnDatalossAsyncOperation.cpp#L82-L126)).
  None of those inherited values proves a fresh peer quorum or serving access.
- The native transactional COM wrapper delegates `GetCurrentProgress` to V1
  ([ComTransactionalReplicator.cpp:508-512](https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/data/txnreplicator/ComTransactionalReplicator.cpp#L508-L512)).
  The separately named logging `GetLastCommittedSequenceNumber` returns current
  log tail, while the stable query returns a different boundary
  ([LoggingReplicatorImpl.cpp:435-445](https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/data/txnreplicator/loggingreplicator/LoggingReplicatorImpl.cpp#L435-L445)).
  Names are not interchangeable transaction durability predicates.
- Native access is separately calculated during promotion/catch-up
  ([ReadWriteStatusCalculator.cpp:58-139](https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Failover/ra/FailoverUnitProxy.ReadWriteStatusCalculator.cpp#L58-L139)).

The pinned Rust description
([stateful_traits.rs:105-108](https://github.com/Azure/service-fabric-rs/blob/7c1ea4e5f9ab5fbe4cfa0b3e9bfaf37169d2b43f/crates/libs/core/src/runtime/stateful_traits.rs#L105-L108))
describes a secondary end-of-log query for selection and catch-up. It does not
establish Kuberic's additional locally durable primary quorum guarantee.
The Kuberic role table above is a deliberate local contract, not a claim of
identical native behavior or full SF compliance.

## Configuration and Quorum

Public configuration contains exact current and optional previous
configuration descriptions. Replication completion requires the configured
write predicates, including both PC and CC while joint authority exists.

Specific-quorum and `All` catch-up are distinct. Advertised peer progress does
not replace an exact live/history-scoped proof.

## Build and Replacement

A build binds source and target replica/storage identities, process sessions,
history, configuration, and attempt ID.

Build completion requires:

- a frozen committed copy boundary;
- deterministic provider copy;
- retained operations through the later catch-up boundary;
- exact terminal completion for the same attempt.

Missing source history requires replacement of the source incarnation.
Conflicting target state is target evidence. A retired incarnation is never
readmitted by higher numeric progress.

### Completed Snapshot Prefix Proof

A snapshot provider need not retain the original operation at its copied
boundary. After successful Build, the source retains an exact session-scoped
completed-build anchor. Active catch-up can ask the live target to certify
that completed committed prefix, including when the configured known peer
omits progress with `INVALID_LSN`. Advertised progress alone grants no credit.

The read-only proof carries the original terminal copy identity separately
from current replication authority. The target requires its exact durable
terminal receipt, live source/target sessions, current active-secondary and
primary/configuration authority, matching history/storage/endpoint and
nonretired storage. Provider commitment, durable applied/committed state and
replicator progress must agree and cover the prefix. The ACK echoes the exact
proof; the source also rechecks its current primary authority before credit
and final quorum success. No application operation is delivered or ACKed by
this proof.

Credit stops at completed catch-up boundary C, bounded by source commitment;
records above C still require contiguous operation-exact replay. Ordinary data
duplicates still require retained matching payloads. A historical final-copy
ACK alone is not a live readiness proof. The source anchor survives activation,
not process replacement; session, epoch, history, storage or endpoint mismatch
prevents its use. A cache miss retains the operation-exact fallback, including
its rebuild-required errors; this is not a general restart-persistent progress
cache or a change to retained-floor semantics.

Native SF's classic V1 replicator supplies copy through S-1 and starts the
replication queue at S. Build waits jointly for final copy completion and its
required replication apply-ACK boundary, then promotion reuses the session's
ACK cursors for catch-up. It does not demand the original record S-1. At pin
`3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d`, see
[BuildIdleAsyncOperation.cpp:112-166](https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Replication/PrimaryReplicator.BuildIdleAsyncOperation.cpp#L112-L166),
[CopySender.cpp:267-400,524-581](https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Replication/CopySender.cpp#L267-L400),
[ReplicaManager.cpp:1671-1785](https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Replication/ReplicaManager.cpp#L1671-L1785)
and [CatchupAsyncOperation.cpp:42-104](https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Replication/PrimaryReplicator.CatchupAsyncOperation.cpp#L42-L104).

Native receiver queue “committed”/received progress is a dispatch cursor,
distinct from service-completed/apply ACK and primary quorum commitment.
Persisted mode requires service ACK; its acknowledgement does not carry
Kuberic's durable application progress or exact storage/history assertion.
See [SecondaryCopyReceiver.cpp:160-287,377-427](https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Replication/SecondaryCopyReceiver.cpp#L160-L287)
and [ReliableOperationSender.cpp:601-679](https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Replication/ReliableOperationSender.cpp#L601-L679).
Kuberic aligns the completion invariant, not these differing ACK/durability
models or native copy EOF behavior.

Implementation: `replicator/stateless/mod.rs:441-472,1160-1234,1625-1764,2174-2203,3032-3057`
and `host/hosting.rs:1084-1115`, under `kuberic-runtime/src/`, at `dbceb59`.
Regression command and scope: [Quiet Snapshot Boundary Proof](testing.md#quiet-snapshot-boundary-proof).

## Service Operation Delivery

Built-in receivers deliver copy chunks, explicit copy completion, and
replication through the service's `StateReplicator` streams. Queue insertion
is not success: ingress completion requires the service's durable ACK, exact
storage/history verification, and still-current authority. Primary-local
apply/commit remains engine-owned and is not echoed to its service stream.

The service takes each stream once and keeps both consumers alive.
CopyComplete ends one snapshot, including an empty snapshot; retained build
replication follows its ACK. `get_operation() == None` means terminal
replica-stream shutdown, not completion of one build or temporary emptiness.
No built-in fallback applies or acknowledges work for an absent consumer.

Services can explicitly call `StreamOperation::apply_to` with their durable
store, then acknowledge its progress or reject its error. This adapter
applies new payloads, verifies duplicate bytes, and advances requested
commitment before returning; it never ACKs implicitly. A duplicate may need
commit without another apply. Custom persistence can use `execute` for the
same cancellation/drain accounting, but must make its own durable assertion.

Copy may preserve an existing intact retained suffix. A truthful larger
CopyComplete progress is accepted only with bounded, contiguous durable
history proof; each retained frame still needs its own validated service ACK.
Physical commitment after rejection is not acknowledged commitment: replay
replies and terminal receipts cannot credit it.

Cancellation invalidates queued work, active cooperative effects, and late
ACKs. Service effects must stay inside the execution lifetime, not detach.
Successor admission and graceful close drain actual active effects; synchronous
work cannot be preempted and durable effects are not rolled back. Pending or
uncertain replication remains access-fenced until a valid current ACK or
verified recovery. Provider durability alone does not publish acknowledged
receiver progress. Rejecting or dropping work never produces transport success.

Exact previously acknowledged copy cursors and durable terminal receipts
can prove historical replay without reapplying. The exact zero-LSN
empty-history probe has no application payload. Neither exception permits
fresh work to bypass service ACK or stale authority to succeed.

Implementation references: `replicator/stream.rs:74-176`,
`replicator/stateless/mod.rs:1105-1168,2099-2417`, and
`host/hosting.rs:71-84` under `kuberic-runtime/src/`.

## Data Loss and History

`on_data_loss` is not trusted as an isolated boolean. Admission validates
history initialization, exact provider/storage identity, session, epoch, and
the authorized attempt.

Distinct histories are incomparable. Election and recovery cannot rank them
by LSN alone.

## Close, Abort, and Tasks

Close is graceful. A replication or application Close error invokes ordered
replicator/application Abort, skips any remaining graceful Close callback,
drains runtime-owned effects, and remains a failed Close. The first child
diagnostic is retained across replay; a cleanup error is additional failure,
not successful graceful shutdown
([retained diagnostic:185-211](../../../kuberic-runtime/src/host/public_lifecycle_program.rs#L185-L211);
[Close and Cleanup:438-503](../../../kuberic-runtime/src/host/public_lifecycle_program.rs#L438-L503)).

Explicit Abort does not await graceful application Close or role callbacks.
Its synchronous hooks fence/terminate their owned work; public terminal
completion additionally waits for built-in listener, inbound/outbound task,
and entered stream-effect drainage. A retained terminal cleanup owner survives
root cancellation and is joined on registry shutdown. The listener join stays
in a shared retained slot, so cancelling graceful Close cannot detach it and
turn an empty listener slot into false containment
([terminal owner:29-74](../../../kuberic-runtime/src/host/public_lifecycle_terminal.rs#L29-L74);
[listener join:431-505](https://github.com/youyuanwu/kuberic/blob/66ad81d139453047f4df2e9282d7b34c79ff7cd9/kuberic-runtime/src/replicator/stateless/mod.rs#L431-L505);
[runtime drain:155-186](https://github.com/youyuanwu/kuberic/blob/66ad81d139453047f4df2e9282d7b34c79ff7cd9/kuberic-runtime/src/replicator/stateless/mod.rs#L155-L186)).

Supersession cancels authority-scoped operations before successor work proceeds.
Root termination or a historical failure record alone is not descendant
containment. A lost callback witness leaves containment pending. Synchronous
native/service effects cannot be forcibly preempted: termination remains
pending until entered work actually exits; already durable effects are not
rolled back. For custom replicators, proof is limited to their synchronous
Abort contract and any supplied callback witness. This does not establish that
all application consumer tasks are joined; SF-15 remains open.

Completed transport-task entries are reclaimed during normal operation; raw
registry cardinality returns to baseline after bounded batches.

### Verified Service Fabric Source and Qualifications

The local read-only Service Fabric checkout was verified at
`3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d` and remained clean during SF-01
revalidation. These are source observations, not guarantees inferred from
Rust-binding comments. The lifecycle rows below are production paths under
`src/prod/src/Reliability/`; the final rows explicitly identify the inspected
production Store pipeline and FabricTest service example.

| Subject | Source evidence and boundary |
|---|---|
| Access versus role | [Failover/ra/FailoverUnitProxy.cpp:1376-1396](https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Failover/ra/FailoverUnitProxy.cpp#L1376-L1396) calculates access; [ComStatefulServicePartition.cpp:482-486](https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Failover/ra/ComStatefulServicePartition.cpp#L482-L486) stores it without invoking application ChangeRole. |
| Actual role order | [ProxyActionsList.cpp:94-105,130-148](https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Failover/ra/ProxyActionsList.cpp#L94-L148) puts replication before application for normal paired transitions and final catch-up before demotion. The finish-demotion list may contain only the remaining application transition. |
| Direct Abort | [FailoverUnitProxy.cpp:428-461](https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Failover/ra/FailoverUnitProxy.cpp#L428-L461) updates access, then aborts replication/application without graceful prerequisites. The distinct [StatefulServiceAbort action list:63-73](https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Failover/ra/ProxyActionsList.cpp#L63-L73) includes setup and role operations; not every SF path named Abort is the direct path. |
| Failed Close | [FailoverUnitProxy.CloseAsyncOperation.cpp:206-219,262-275](https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Failover/ra/FailoverUnitProxy.CloseAsyncOperation.cpp#L206-L275) aborts failed Close children, but its [outer completion:94-105](https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Failover/ra/FailoverUnitProxy.CloseAsyncOperation.cpp#L94-L105) returns success. Kuberic deliberately preserves the failed outer Close instead. |
| Owned Abort work | [Replication/PrimaryReplicator.cpp:275-315](https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Replication/PrimaryReplicator.cpp#L275-L315) cancels catch-up and synchronously waits for replication callbacks. This supports an owned completion boundary, not wait-free Abort or forced termination of arbitrary application code. |
| Catch-up access timing | [ReadWriteStatusCalculator.cpp:73-81,116-125,312-345](https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Failover/ra/FailoverUnitProxy.ReadWriteStatusCalculator.cpp#L73-L345) permits reads during catch-up and phases write denial. Kuberic denies both read/write access at program launch, a more conservative policy, not identical SF timing. |
| Read admission is not per-apply isolation | [ReadWriteStatusCalculator.cpp:58-99](https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Failover/ra/FailoverUnitProxy.ReadWriteStatusCalculator.cpp#L58-L99) grants ReadyPrimary reads, keeps reads during demotion/no-primary-change catch-up, and denies idle-to-active secondary reads on this classic path. This is lifecycle/reconfiguration calculation, not Kuberic's per-write applied/committed fence. |
| Public status observation | [ComStatefulServicePartition.cpp:75-147](https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Failover/ra/ComStatefulServicePartition.cpp#L75-L147) reads cached access under a lock, checks openness and adjusts for lease expiry. It does not lock an arbitrary service's data view. |
| Replication admission/completion | [Replicator.ReplicateAsyncOperation.cpp:43-64](https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Replication/Replicator.ReplicateAsyncOperation.cpp#L43-L64) checks partition write access before internal primary state and starts replication; [completion:103-108](https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Replication/Replicator.ReplicateAsyncOperation.cpp#L103-L108) ends replication. This is not generic application-memory commit or universal read isolation. |
| Service-owned visibility example (test service) | [src/prod/test/FabricTest/TestStoreService.cpp:108-155](https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/test/FabricTest/TestStoreService.cpp#L108-L155) stages next value/version separately from its locked readable value; [791-817](https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/test/FabricTest/TestStoreService.cpp#L791-L817) commits after successful EndReplicate or rolls back. [866-883](https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/test/FabricTest/TestStoreService.cpp#L866-L883) separately checks partition read status. This illustrates that service, not a universal SF application guarantee. |
| Production service local commit | [src/prod/src/Store/ReplicatedStore.TransactionReplicator.cpp:719-731](https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Store/ReplicatedStore.TransactionReplicator.cpp#L719-L731) checks replication success before starting local commit; [848-857](https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Store/ReplicatedStore.TransactionReplicator.cpp#L848-L857) begins the local transaction commit. This establishes this Store pipeline, not arbitrary provider isolation. |

SF's symbolic `CatchupReplicaSetAll` action can select WriteQuorum when specific
quorum is supported, otherwise All
([ReplicatorCatchupReplicaSetAsyncOperation.cpp:107-118](https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Failover/ra/FailoverUnitProxy.ReplicatorCatchupReplicaSetAsyncOperation.cpp#L107-L118)).
Names alone do not establish mode or full semantic equivalence.

## Conformance

The KSF-01 through KSF-16 catalog maps these semantics to ordinary runtime,
controller, provider, PostgreSQL, and build-only tests. KSF-09/10/16 also
validate the production ownership inventory and reject alternate endpoints,
protocols, or controller owners.

See [Testing](testing.md) for current selectors and counts.

Known gaps against these contracts and their remediation status are tracked in
the [Service Fabric Contract Audit Tracker](service-fabric-contract-audit.md).

## Unsupported Semantics

Kuberic does not provide:

- migration or compatibility with removed stores/protocols;
- rolling mixed-version participation;
- Public V1 transport authentication;
- transparent client-request continuation across process loss;
- a V2 transaction/checkpoint/backup contract.
