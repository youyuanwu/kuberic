# kuberic-runtime

Application and replication runtime for the level-triggered Kuberic stack.

## Service Fabric V1 interfaces and ownership

The public traits follow the **V1 COM divisions**, not an expanded
“SF-inspired” callback API. Begin/End pairs become async Rust methods; Abort
remains synchronous. The authoritative definitions are
`FabricRuntime.idl:495–539,577–633,687–758`.

| Rust interface | Methods |
|---|---|
| `StatefulServiceReplica` | `open`, `change_role`, `close`, `abort` |
| `Replicator` | `open` → replication address, `change_role(epoch, role)`, `update_epoch`, `close`, `abort`, `current_progress`, `catch_up_capability` |
| `PrimaryReplicator: Replicator` | `on_data_loss`, `update_catch_up_replica_set_configuration`, `wait_for_catch_up_quorum`, `update_current_replica_set_configuration`, `build_replica`, `remove_replica` |
| `StateReplicator` | `replicate(operation_data)` → committed LSN, `get_replication_stream`, `get_copy_stream`, `update_replicator_settings` |
| `StateProvider` | `update_epoch(epoch, previous_epoch_last_lsn)`, `last_committed_lsn`, `get_copy_context`, `get_copy_state`, `on_data_loss` |

Service Open receives a `StatefulServicePartition` in its `OpenContext`.
The service selects a `ReplicatorFactory` with `partition.with_factory(...)`,
then calls `create_replicator(state_provider, settings)`. The result contains
the control and state-replicator interfaces; the optional primary interface is
the explicit Rust counterpart of querying `IFabricPrimaryReplicator`.

```text
PodRuntime ── Open(partition) ──► StatefulServiceReplica
                                  │
                                  ├─ CreateReplicator(StateProvider, settings)
                                  ├─ retains StateReplicator
                                  ├─ takes and consumes copy/replication streams
PodRuntime ◄── returns Replicator ─┘
     │
     └─ opens and drives exactly the returned control interface
```

Choose the built-in implementation with
`DefaultReplicatorFactory::new(storage)`, where `storage` implements the
non-COM `engine::DurableState` persistence adapter. The state provider may be a
separate object. Custom factories use the same partition boundary and need not
use the default engine or implement `DurableState` on the service.
`CreateReplicator` is the construction boundary: the default factory creates
one shared `DefaultReplicatorInner`, and the control, primary, state, and
optional managed façades all reference that inner. `PodRuntime` owns the
hosting registrar, application lifetime, Open registration, effect ordering,
and exact returned-interface identity; it does not preconstruct unused default
replication state for a custom factory.
There is no `PodRuntime::new_with_replicator` ownership shortcut.
Services can observe `partition.get_write_status()`; custom factories receive
the same access gate through `ReplicatorFactoryContext::write_status`.
Custom implementations must honor that gate rather than infer write access
from the Primary role.

`ReplicatorFactoryContext` exposes stable identity and partition-access
capabilities, not a concrete runtime or default-engine pointer. Application
and custom-factory code constructs only the SF-shaped interface bundle through
`ReplicatorInterfaces::new`. The default implementation's managed data-plane
bridge is crate-private; custom replicators own their transport independently.

## Durable streams and engine integration

`StateProvider` copy callbacks exchange `OperationDataStream`s (opaque byte
buffers), not durable delivery acknowledgements. `StateReplicator` returns
service-owned `OperationStream`s. Each delivered `StreamOperation` carries
metadata, data, and a one-shot acknowledgement:

- Persist the operation or copy boundary before calling `acknowledge(progress)`.
- `reject(error)` reports a failed application operation.
- Dropping an operation is **not** success; the waiting delivery fails.
- The engine validates durable progress and commits authority/build metadata
  before returning an applied peer ACK. Close/Abort and runtime drop terminate
  outstanding deliveries.

Inbound replication exposes two acknowledgements. `PendingReplication::received`
is available after ordered receiver admission and may advance transport resend
state without granting quorum credit. `PendingReplication::applied()` completes
only after durable service acceptance; only its applied progress is eligible
for quorum accounting.

`OperationStream::channel` supplies the producer/consumer boundary for custom
engines. Streams can be taken only once from the default state interface.

Reservations, exact-authority admission and ACK validation, queue retention,
quorum finalization, copy/build bookkeeping, and durable retry IDs live in the
non-COM replication engine, not the SF traits. The default state interface
retains write identity across failures and cancellation. The lower-level
`PodRuntime` data-plane methods are compatibility forwarders to the selected
managed replicator; they fail explicitly for a custom implementation that does
not opt into that capability.

Transport remains caller-driven: drain `PodRuntime::next_outbound()` for
state-interface replication and primary build/remove requests, exchange the
exact-authority wire items, and return their ACKs. Build requests use the
existing `prepare_copy`/copy-ACK path; `build_replica` does not complete merely
because a request was queued. Logical outbound work uses a bounded,
abort-aware queue; network resend windows and reconnect remain agent-owned.
The runtime does not start a network server.

Role changes drive the replicator before the service callback. The completed
role is published only after both callbacks succeed; an in-process
`RoleTransition` exposes partial completion after failure. Close fences writes,
closes the replicator, then closes the service, with abort cleanup on callback
failure. Failed or cancelled Open aborts created interfaces;
lifecycle/epoch failures never reopen writes.

Hosting/effect compatibility types are hidden from the generated user
documentation. They exist until `kuberic-agent` owns process hosting and
durable effect execution; they are not application programming-model APIs.
Runtime role and write access remain separate: startup is write-closed,
becoming Primary does not grant writes, and direct client writes require an
explicit granted `WriteStatus`.

The runtime does not create an operator, replica agent, or control-plane
server. Persistence is exposed as narrow `ReplicaAuthorityStore`,
`ReplicationProgressStore`, `LocalWriteJournal`, `BuildAuthorityStore`, and
`BuildProgressStore` capabilities. `AuthorityStore` is only their composite
compatibility bound; one future SQLite implementation may implement them all
without granting every caller every mutation right.

Replica builds use separate exact-target authority outside quorum membership.
Copy context remains a multi-item operation-data stream. `prepare_copy` returns
a bounded stream that incrementally carries snapshot chunks, the captured copy
boundary, and subsequent live replication without holding the global runtime
effect lock across provider enumeration. Durable duplicate snapshot chunks are
verified and acknowledged without redelivery to the application.

## Deferred Service Fabric completion contracts

This crate provides data-plane primitives, not end-to-end SF failover. Later
owners must provide:

- a reliable transport session with a fresh endpoint incarnation, stale-session
  rejection, ordered delivery, reconnect, cancellation, and endpoint readiness;
- runtime-domain replication messages and agent-side protobuf conversion,
  replacing the temporary `kuberic-runtime -> kuberic-wire` dependency and
  `PodRuntime` compatibility forwarding;
- a named resend-retention owner whose actual retained range determines
  catch-up capability, truncation, and fallback to full copy;
- durable agent-owned effect intent/result sequencing across process restart;
- the FM/RA-equivalent coordinator that persists and resumes demote, GetLSN,
  catch-up, deactivate, and activate stages;
- operation-specific mappings for removal, cancellation, backpressure, and
  transient reconfiguration outcomes.

Until those owners are implemented, the crate is an SF-aligned runtime
foundation rather than a deployable SF-equivalent failover system.
