# Replicator Boundary

Kuberic hosts built-in and application-owned Replicators through the same
Service Fabric-shaped public interfaces.

The sole built-in factory composes the source-guided native classic V1 owners.
See the [runtime guide](classic-v1-runtime.md) for assignment, stream, controller,
transport and restart behavior, with direct selected source/test evidence.
The [pending-items plan](sf-v1-semantic-alignment-plan.md) separates verified
core differences, unverified qualification and deferred capability work.

## Public Interfaces

- `ReplicatorFactory` / `ReplicatorFactoryContext`;
- `Replicator` / `PrimaryReplicator`;
- optional `StateReplicator`;
- optional `StateProvider`;
- optional local `CustomReplicatorControl` companion for independent native engines;
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
has its own ACK rules. An aborted reader sees typed `RuntimeError::Closed`
once before termination, distinct from graceful EOF. Missing consumers do not
manufacture required service ACKs. Cancelled
observers do not cancel accepted native or application work.

## Public Completion Evidence

Successful public calls certify their documented postconditions. Kuberic may
persist that an exact call completed, but it does not promote private
transport state into controller evidence.

- configuration completion means the exact PC/CC description is installed;
- catch-up completion means the requested predicate holds;
- build completion means copy plus retained catch-up completed;
- remove completion releases the exact idle membership/retention target, not
  arbitrary provider callbacks or remote storage;
- data-loss completion is validated against exact history/provider evidence;
- direct Close, Abort containment requests and separate task/provider joins
  have distinct postconditions; a cancelled observer proves none of those joins.

The [lifecycle evidence](classic-v1-runtime.md#host-access-lifecycle-and-retirement)
documents these limits and the selected outer-Close/native outcome difference.

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

### Custom controller integration

An independent factory attaches its own `CustomReplicatorControl` through
`ReplicatorInterfaces::with_custom_control(&context, owner)` while holding its
one-shot factory context. Return that same coherent creation. Attachment and
registration must agree on the exact owner; an abandoned or different returned
bundle cannot discard pending cleanup ownership. `NativeRuntimeControl`, its
TCP/queue diagnostics and host/store setters remain private. Wrapping a built-in
bundle is not an independent native-engine implementation.

The companion is a **Kuberic local authority adaptation**, not a new method on
SF's Replicator, PrimaryReplicator or StateProvider. Its responsibilities are:

| Contract | Required meaning |
|---|---|
| `install_inbound_build_authority` | Install the exact target copy ingress owner before acknowledging success. The controller awaits this acknowledgement before dispatching source Build. |
| `revoke_inbound_build_authority` | Invalidate only the matching attempt's ingress. Stale revocation must not cancel another owner; revocation is not voter removal, data deletion or proof of receiver drainage. |
| `idle_replicas(configuration)` | Observe real native idle membership coherently with the supplied last successfully installed host configuration. An error means unavailable, not observed empty membership. |
| `shutdown_and_drain` | Fence and join the creation's actual owned native descendants. Failed/lost cleanup is not positive containment. |

The host retains serialized install/revoke actions independently of an RPC
waiter, invalidates authority before awaiting cleanup, and retains receiver
ownership after revocation. A different receiver attempt cannot reset an
uncertain receiver in the same creation; containment/process replacement is
required. Exact current install replay does not duplicate installation.
Historical source completion is distinct from a live ingress grant; revoked
installation replay cannot reauthorize copy. Failed/interrupted custom callbacks
can remain `ContainmentPending` until actual terminal cleanup. Neither a return
code nor cancellation fabricates a join.

Protected Close/Abort must independently notify or cancel in-flight companion
actions before awaiting serialized drain. An installer that can be released
only by its own drain creates a dependency cycle and violates this contract.
Successful native joining is retained once; later cleanup is exact
compare-and-revoke and cannot repeat effects against replacement-owned storage.
Retained compensation joins and failures are shared by concurrent and later
shutdown observers; losing one response cannot convert a failed cleanup into
successful containment.

Use `ReplicatorFactoryContext::outbound_build_authority(build_id)` inside the
accepted source Build to obtain its complete frozen authority. Revalidate with
`validate_build_authority` before native effects, after awaits and before
publishing completion. This validates the **local** registered creation and
source journal or target grant; it is not a distributed liveness oracle.
The application-native protocol must also validate the other endpoint. A copied
authority object or a credential is not self-authorizing.

Bindings include resource/history and their generations, both replica
incarnations, storage incarnations, process sessions, endpoints, epoch,
configuration, Build ID, operation ID and revision. The canonical source input
digest remains bound by the host's accepted operation, not a fabricated extra
field in native callbacks or receipts.

Local validation checks the coherent frozen source snapshot and the current
operation's cancellation/fence state after awaits. Target validation checks
both durable and process-local exact ingress ownership; a suspended reader
cannot combine an old grant with a later revoked/fenced lifetime.

Reports combine a successful exact host configuration with the companion's
coherent native idle sample. Missing, failed, stale, torn, duplicate or
voting-overlap observations cannot become installed evidence. A successfully
journaled **secondary host configuration** may precede primary native peer
admission: it is control authority, not a nonexistent secondary
PrimaryReplicator callback, native peer completion, or serving access.

PostgreSQL supplies an actual independent
[native controller witness](../../../examples/postgres/src/public_controller_tests.rs).
The external runtime contract fixture is contract/negative evidence only.
Unpublished downstream MySQL fixtures and the externally reported
seven-completion trace are not reproduced by either fixture; downstream engines
must requalify their own native transfer, lineage, replay, admission and fencing.

Source anchors: [attachment:599–626](../../../kuberic-runtime/src/replicator/mod.rs#L599-L626),
[read-only context:935–960](../../../kuberic-runtime/src/replicator/mod.rs#L935-L960),
[local validation:1365–1558](../../../kuberic-runtime/src/host/hosting.rs#L1365-L1558),
[retained actions:600–793](../../../kuberic-runtime/src/replicator/custom.rs#L600-L793),
and [configuration evidence:892–940](../../../kuberic-runtime/src/host/public_lifecycle.rs#L892-L940).
The [PG companion:547–641](../../../examples/postgres/src/public_v1.rs#L547-L641)
and [controller cases:185–1412](../../../examples/postgres/src/public_controller_tests.rs#L185-L1412)
are concrete engine/qualification references, not new native SF interfaces.
