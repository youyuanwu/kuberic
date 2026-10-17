# Service Fabric Stateful API Semantics and Kuberic Conformance

## Scope

This document summarizes the Service Fabric V1 semantics implemented by
Kuberic's Public V1 runtime. It does not define the Service Fabric V2
Transactional Replicator.

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
[listener join:431-505](../../../kuberic-runtime/src/replicator/stateless/mod.rs#L431-L505);
[runtime drain:155-186](../../../kuberic-runtime/src/replicator/stateless/mod.rs#L155-L186)).

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
`3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d`. These are production-source
observations, not guarantees inferred from Rust-binding comments. Paths below
are under `src/prod/src/Reliability/` at that exact revision.

| Subject | Source evidence and boundary |
|---|---|
| Access versus role | [Failover/ra/FailoverUnitProxy.cpp:1376-1396](https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Failover/ra/FailoverUnitProxy.cpp#L1376-L1396) calculates access; [ComStatefulServicePartition.cpp:482-486](https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Failover/ra/ComStatefulServicePartition.cpp#L482-L486) stores it without invoking application ChangeRole. |
| Actual role order | [ProxyActionsList.cpp:94-105,130-148](https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Failover/ra/ProxyActionsList.cpp#L94-L148) puts replication before application for normal paired transitions and final catch-up before demotion. The finish-demotion list may contain only the remaining application transition. |
| Direct Abort | [FailoverUnitProxy.cpp:428-461](https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Failover/ra/FailoverUnitProxy.cpp#L428-L461) updates access, then aborts replication/application without graceful prerequisites. The distinct [StatefulServiceAbort action list:63-73](https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Failover/ra/ProxyActionsList.cpp#L63-L73) includes setup and role operations; not every SF path named Abort is the direct path. |
| Failed Close | [FailoverUnitProxy.CloseAsyncOperation.cpp:206-219,262-275](https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Failover/ra/FailoverUnitProxy.CloseAsyncOperation.cpp#L206-L275) aborts failed Close children, but its [outer completion:94-105](https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Failover/ra/FailoverUnitProxy.CloseAsyncOperation.cpp#L94-L105) returns success. Kuberic deliberately preserves the failed outer Close instead. |
| Owned Abort work | [Replication/PrimaryReplicator.cpp:275-315](https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Replication/PrimaryReplicator.cpp#L275-L315) cancels catch-up and synchronously waits for replication callbacks. This supports an owned completion boundary, not wait-free Abort or forced termination of arbitrary application code. |
| Catch-up access timing | [ReadWriteStatusCalculator.cpp:73-81,116-125,312-345](https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Failover/ra/FailoverUnitProxy.ReadWriteStatusCalculator.cpp#L73-L345) permits reads during catch-up and phases write denial. Kuberic denies both read/write access at program launch, a more conservative policy, not identical SF timing. |

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
