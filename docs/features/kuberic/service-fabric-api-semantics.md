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

## Unsupported Semantics

Kuberic does not provide:

- migration or compatibility with removed stores/protocols;
- rolling mixed-version participation;
- Public V1 transport authentication;
- transparent client-request continuation across process loss;
- a V2 transaction/checkpoint/backup contract.
