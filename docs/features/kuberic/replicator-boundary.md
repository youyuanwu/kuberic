# Replicator Boundary

Kuberic hosts built-in and application-owned Replicators through the same
Service Fabric-shaped public interfaces.

This page describes today's ownership. The
[faithful native V1 Rust async/tonic port design](sf-v1-semantic-alignment-plan.md)
plans source-guided queue/session/state-machine replacement with service-owned
primary commit, independent assignment/completion and receiver-lifetime streams.
Those changes are not implemented yet; the existing
engine-owned primary persistence and durable-ACK rules below remain current.

## Public Interfaces

- `ReplicatorFactory` / `ReplicatorFactoryContext`;
- `Replicator` / `PrimaryReplicator`;
- optional `StateReplicator`;
- optional `StateProvider`;
- built-in persistence adapter `DurableState`.

The protected method sets are unchanged. Applications receive no agent store,
authority setter, Kubernetes client, or private lifecycle capability.

## Sole Built-In Path

`DefaultReplicatorFactory` constructs the stateless Public V1 engine. There is
no managed factory, engine selector, preview selector, or fallback.

One creation reservation binds:

- exact replica and process identity;
- one public interface bundle;
- one replication listener;
- at most one provider writer;
- one cancellation/abort lifetime.

Crossed identities, duplicate registration, listener mismatch, or abandoned
Open abort the creation.

## Durable Ownership

The local agent persists identity, Public V1 lifecycle/operation authority,
history, build/removal/retirement requirements, and terminal completion.

The provider persists application state, applied/committed progress, retained
operations, and copy snapshots.

The Replicator owns live queues, peers, ACK aggregation, copy/replication
streams, and authority-scoped tasks. Those process-local objects are rebuilt
after restart.

Receiver copy/replication effects belong to the service consuming the built-in
streams, not a second direct engine persistence path. Explicit durable service
ACK and exact post-ACK verification precede successful transport completion.
Primary-local persistence remains engine-owned. The optional
`StreamOperation::apply_to` adapter is explicitly service-invoked; missing
consumers never receive an implicit success fallback.

Both streams live for the replica lifetime. CopyComplete is a per-build
operation, not EOF. Service effects use cancellation-aware execution and
remain owned until drained; cancelled requests do not imply durable rollback.
See [service operation delivery](service-fabric-api-semantics.md#service-operation-delivery).

## Public Completion Evidence

Successful public calls certify their documented postconditions. Kuberic may
persist that an exact call completed, but it does not promote private
transport state into controller evidence.

- configuration completion means the exact PC/CC description is installed;
- catch-up completion means the requested predicate holds;
- build completion means copy plus retained catch-up completed;
- remove completion means the exact idle target is released;
- data-loss completion is validated against exact history/provider evidence;
- Close/Abort drain or contain descendants.

Incomparable histories return no ordering. Numeric progress cannot substitute
for history identity.

## Provider Writer

The host owns an exactly-one-writer reservation for the provider. The count is
zero before admitted Open, one after current Open, unchanged by replay, and
zero after stale/superseded containment.

This local ownership mechanism is not a Kubernetes Lease and is unrelated to
production cutover.

## Transport Boundary

The Replicator owns its replication endpoint lifetime. Public V1 transport is
credential-free. Identity/session/epoch/history/storage/build values fence
correctness but are not authentication.

The bearer token protects the separate agent command endpoint. Kubernetes
namespace RBAC protects controller resource mutation.

## Custom Replicators

Custom Replicators use the same public lifecycle bundle and may own their own
transport and durable formats. PostgreSQL is the repository example.

Custom implementations must:

- honor partition read/write status;
- reject stale authority and unsupported provider state;
- preserve exact process/session/storage fencing;
- return the same coherent public interface allocation.

They do not receive a compatibility adapter for the removed built-in engine.
