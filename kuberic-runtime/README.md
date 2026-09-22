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
There is no `PodRuntime::new_with_replicator` ownership shortcut.
Services can observe `partition.get_write_status()`; custom factories receive
the same access gate through `ReplicatorFactoryContext::write_status`.
Custom implementations must honor that gate rather than infer write access
from the Primary role.

## Durable streams and engine integration

`StateProvider` copy callbacks exchange `OperationDataStream`s (opaque byte
buffers), not durable delivery acknowledgements. `StateReplicator` returns
service-owned `OperationStream`s. Each delivered `StreamOperation` carries
metadata, data, and a one-shot acknowledgement:

- Persist the operation or copy boundary before calling `acknowledge(progress)`.
- `reject(error)` reports a failed application operation.
- Dropping an operation is **not** success; the waiting delivery fails.
- The engine validates durable progress and commits authority/build metadata
  before returning a peer ACK. Close/Abort and runtime drop terminate outstanding
  deliveries.

`OperationStream::channel` supplies the producer/consumer boundary for custom
engines. Streams can be taken only once from the default state interface.

Reservations, exact-authority admission and ACK validation, queue retention,
quorum finalization, copy/build bookkeeping, and durable retry IDs live in the
non-COM replication engine, not the SF traits. The default state interface
retains write identity across failures and cancellation. The lower-level
`PodRuntime::begin_write(ClientWrite)` additionally accepts caller-owned retry
IDs and returns a `PendingWrite` with transport items and a durable completion.

Transport remains caller-driven: drain `PodRuntime::next_outbound()` for
state-interface replication and primary build/remove requests, exchange the
exact-authority wire items, and return their ACKs. Build requests use the
existing `prepare_copy`/copy-ACK path; `build_replica` does not complete merely
because a request was queued. The runtime does not start a network server.

Promotion drives the replicator before the service callback. Demotion fences
writes, calls the service, then changes the replicator role. Close fences
first, closes the service, then closes the replicator. Failed or cancelled
Open aborts created interfaces; lifecycle/epoch failures never reopen writes.

The crate also provides a caller-driven `PodRuntime`, durable authority
admission, ordered idempotent effects, exact-incarnation replication, and PC/CC
quorum tracking. Runtime role and write access are separate: startup is
write-closed, becoming Primary does not grant writes, and direct client writes
require an explicit granted `WriteStatus`.

The runtime does not create an operator, replica agent, or control-plane
server. A caller supplies `RuntimeControlPlane` and `AuthorityStore`
implementations. Replication acknowledgements are accepted only when exact
identity, epoch, and configuration fences match durable authority.

Replica builds use separate exact-target authority outside quorum membership.
Copy closes at a captured replication boundary, then a live build lane carries
new writes until the target joins configuration authority.
