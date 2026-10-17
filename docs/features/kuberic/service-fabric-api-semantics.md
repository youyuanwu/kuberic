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

Implementation references: `replicator/stream.rs:72-160`,
`replicator/stateless/mod.rs:1073-1136,2120-2327`, and
`host/hosting.rs:71-84` under `kuberic-runtime/src/`.

## Data Loss and History

`on_data_loss` is not trusted as an isolated boolean. Admission validates
history initialization, exact provider/storage identity, session, epoch, and
the authorized attempt.

Distinct histories are incomparable. Election and recovery cannot rank them
by LSN alone.

## Close, Abort, and Tasks

Close is graceful and drains owned descendants. Abort fences synchronously and
contains remaining work. Supersession cancels authority-scoped operations
before successor work proceeds.

Completed transport-task entries are reclaimed during normal operation; raw
registry cardinality returns to baseline after bounded batches.

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
