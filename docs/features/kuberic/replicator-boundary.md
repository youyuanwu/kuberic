# Replicator Boundary

**CURRENT ordinary integration:** [M1](ordinary-hosting-m1.md) uses a coherent
ordinary factory bundle with read-only generic peer discovery, no mandatory
companion/diagnostics, retained ordinary Close after Abort and exact host
goals/receipts. Protected replication/provider method sets are unchanged.
Built-in/KV/SQLite/PostgreSQL production integration explicitly rejects pending
M2. The legacy companion/grant APIs described below remain for engine/component
consumers awaiting M2, not a production fallback. They must not be removed from
those consumers by simply deleting their fences.

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
In the historical pre-M1 route, companion attachment was optional for an
interface bundle, but hosted controller Build rejected a creation without native runtime control
([pinned host:891–927](https://github.com/youyuanwu/kuberic/blob/ce9df4ad85603b935752ed7184d9314a778b6eb5/kuberic-runtime/src/host/hosting.rs#L891-L927)).
M1 replaces that production dependency, not the actual M2 native transfer rules.

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

**Historical engine/component contract, not the M1 production route.** Use
[ordinary M1](ordinary-hosting-m1.md) for new generic integration. The target
M2 consumer replacement must qualify native sessions/effects/joins before
deleting these remaining APIs and fixtures; PG metadata removal stays separate.

An independent factory attaches its own `CustomReplicatorControl` through
`ReplicatorInterfaces::with_custom_control(&context, owner)` while holding its
one-shot factory context. Return that same coherent creation. Attachment and
registration must agree on the exact owner; an abandoned or different returned
bundle cannot discard pending cleanup ownership. `NativeRuntimeControl`, its
TCP/queue diagnostics and host/store setters remain private. Wrapping a built-in
bundle is not an independent native-engine implementation.

The companion is a **current Kuberic extension-boundary divergence**, not a new
method on SF's Replicator, PrimaryReplicator or StateProvider. Calling it a local
adaptation does not establish that its mandatory controller integration is
necessary. The proposed target removes it with replacement host/engine safety
owners; the responsibilities below remain required **today**:

| Contract | Required meaning |
|---|---|
| `callback_ownership()` | Immutable engine-only interruption contract; conservative object-owned default. NativeObserver does not certify application callbacks, repeatability or successful drainage. |
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

Reports publish successful exact host configuration independently of the
optional coherent `engine_idle` sample. Missing, failed, stale, torn, duplicate
or voting-overlap observations suppress only that sample; `None` is not
observed empty. Exact `remote_builds` are host journal state, not native idle
membership or a new grant. A successfully
journaled **secondary host configuration** may precede primary native peer
admission: it is control authority, not a nonexistent secondary
PrimaryReplicator callback, native peer completion, or serving access.

PostgreSQL supplied a pinned pre-M1 independent
[native controller witness](https://github.com/youyuanwu/kuberic/blob/ce9df4ad85603b935752ed7184d9314a778b6eb5/examples/postgres/src/public_controller_tests.rs).
Current hosted wrappers check only M2 unavailability.
The external runtime contract fixture is contract/negative evidence only.
Unpublished downstream MySQL fixtures and the externally reported
seven-completion trace are not reproduced by either fixture; downstream engines
must requalify their own native transfer, lineage, replay, admission and fencing.

Both retained engine/component consumers use the [same coordinator](https://github.com/youyuanwu/kuberic/blob/ce9df4ad85603b935752ed7184d9314a778b6eb5/kuberic-runtime/src/replicator/coordinator.rs#L1).
Adapters provide actual ingress/drain effects and optional diagnostics, not
engine-selected authority policy. Pre-M1 production returned-failure retry
proof was Never; unsafe or undeclared callback failures required containment.
See the [historical message/state mapping and limits](engine-neutral-hosting.md#current--delivered-bounded-hosting).

Immutable pre-M1 source anchors:
[attachment](https://github.com/youyuanwu/kuberic/blob/ce9df4ad85603b935752ed7184d9314a778b6eb5/kuberic-runtime/src/replicator/mod.rs#L596),
[legacy Build validation:938–951](https://github.com/youyuanwu/kuberic/blob/ce9df4ad85603b935752ed7184d9314a778b6eb5/kuberic-runtime/src/replicator/mod.rs#L938-L951),
[local validation:1365–1558](https://github.com/youyuanwu/kuberic/blob/ce9df4ad85603b935752ed7184d9314a778b6eb5/kuberic-runtime/src/host/hosting.rs#L1365-L1558),
[retained actions](https://github.com/youyuanwu/kuberic/blob/ce9df4ad85603b935752ed7184d9314a778b6eb5/kuberic-runtime/src/replicator/coordinator.rs#L173),
and [configuration evidence](https://github.com/youyuanwu/kuberic/blob/ce9df4ad85603b935752ed7184d9314a778b6eb5/kuberic-runtime/src/host/public_lifecycle.rs#L1044).
The [PG companion:547–641](https://github.com/youyuanwu/kuberic/blob/ce9df4ad85603b935752ed7184d9314a778b6eb5/examples/postgres/src/public_v1.rs#L547-L641)
and [controller cases:185–1412](https://github.com/youyuanwu/kuberic/blob/ce9df4ad85603b935752ed7184d9314a778b6eb5/examples/postgres/src/public_controller_tests.rs#L185-L1412)
are concrete engine/qualification references, not new native SF interfaces.
