# Replicator Boundary

Kuberic hosts built-in and application-owned Replicators through the same
Service Fabric-shaped public interfaces.

The sole built-in factory composes the source-guided native classic V1 owners.
See the [runtime guide](classic-v1-runtime.md) for assignment, stream, controller,
transport and restart behavior. The [port plan](sf-v1-semantic-alignment-plan.md)
retains the source baseline and intentionally separate parity/qualification gates.

## Public Interfaces

- `ReplicatorFactory` / `ReplicatorFactoryContext`;
- `Replicator` / `PrimaryReplicator`;
- optional `StateReplicator`;
- optional `StateProvider`;
- application-owned persistence; `DurableState` is not a factory prerequisite.

The protected method sets are unchanged. Applications receive no agent store,
authority setter, Kubernetes client, or private lifecycle capability.

## Sole Built-In Path

`DefaultReplicatorFactory::new()` constructs the classic V1 composition. There is
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
after restart; native secondary readiness requires real copy.

Receiver copy/replication effects belong to the service consuming the built-in
streams, not a second direct engine persistence path. Persisted services durably
accept before parameterless ACK. Volatile dispatch can complete without service
ACK. Primary preparation, ordered commitment, publication and client success
are service-owned and separate from native quorum completion.

Streams share the native receiver lifetime; an explicit end-of-stream operation
has its own ACK rules. Retirement is typed `RuntimeError::Closed`, not successful
EOF. Missing consumers do not manufacture required service ACKs. Cancelled
observers do not cancel accepted native or application work.

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

The Replicator owns its bounded tonic carrier endpoint lifetime. The host's
bearer credential authenticates peer connections; exact process, storage,
history, configuration and endpoint bindings independently fence each delivery.
The control endpoint also uses the bearer credential. Neither endpoint supplies
TLS; Kubernetes namespace RBAC and deployment network isolation remain necessary.

## Custom Replicators

Custom Replicators use the same public lifecycle bundle and may own their own
transport and durable formats. PostgreSQL is the repository example.

Custom implementations must:

- honor partition read/write status;
- reject stale authority and unsupported provider state;
- preserve exact process/session/storage fencing;
- return the same coherent public interface allocation.

They do not receive a compatibility adapter for the removed built-in engine.
