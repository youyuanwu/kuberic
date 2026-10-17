# SQLite on a V2 Transactional Replicator

## Status and Scope

Proposed application integration with the
[faithful Rust port of SF V2](../kuberic/v2-transactional-replicator-design.md).
No V2 transactional engine or SQLite V2 provider is implemented by this
document. The [current SQLite application](design.md) remains on Kuberic's
Public V1 durability path.

Here transactional V2 is distinct from the existing level-triggered v2
host/operator. The shared design preserves native SF component boundaries,
transaction processing, barriers, checkpoint/recovery and copy semantics.
SQLite must fit those semantics; it must not turn the engine into a
committed-only, full-copy-only application-specific scheduler.

SQLite can use one provider and one atomic redo operation per SQL transaction.
That is an application choice, not a limit on the engine's regular
transactions, undo-capable providers, dynamic registry or copy modes.

## Goals and Non-Goals

Move generic logging, transaction/operation identity, retained history,
recovery, progress vectors, checkpoint coordination, copy selection and
backup/restore into the ported V2 components. Keep deterministic WAL-frame
interpretation, SQLite image creation, materialization, SQL connection
management and local WAL publication in the provider/service.

The SQL success boundary remains successful V2 atomic completion **and**
successful local SQLite publication under current authority. Success does not
add a requirement to wait for SF's last stable LSN.

Non-goals are SQL replay on secondaries, multi-primary SQLite, arbitrary
attached-database/extension replication, cross-partition transactions,
automatic client exactly-once execution, and retaining the old frame log as
an independent recovery authority.

Preserve `Replicator`, `PrimaryReplicator`, and `StateProvider` unchanged.
SQLite implements a separate `StateProvider2`-equivalent Rust interface.

## Current and Target Ownership

The existing application has an immutable base plus durable WAL-frame history,
progress/copy metadata, and live serving files:

```text
application/
  state-v2.json
  base-<generation>.sqlite
  history-<generation>.log
  db.sqlite
  db.sqlite-wal
  db.sqlite-shm
```

`SqlitePersistence` implements `DurableState`; `DurableFrameLog` owns retained
history, progress and copy staging. The commit-barrier VFS captures complete
WAL transactions and withholds their commit frames while replication runs.
See `examples/sqlite/src/state.rs:210-303`,
`examples/sqlite/src/service.rs:25-69`, and
`examples/sqlite/src/commit_barrier/mod.rs:1-59`.

| Concern | Target owner |
|---|---|
| Identity, topology, epochs, configuration and serving authority | Existing agent/controller |
| Transport sessions, PC/CC quorums and V1 lifecycle | Existing V1 engine through private V2 bridge |
| Transaction records, append/flush, transaction map, barriers and progress vectors | Ported `txnreplicator::loggingreplicator` / `logrecordlib` |
| Provider name/ID/type, transactional membership and hierarchy | Ported `txnreplicator::statemanager` |
| SQLite page interpretation, candidate materialization and checkpoint images | SQLite `StateProvider2` |
| Recovery cutoff, replay, copy mode, truncation and backup log chain | Ported V2 framework |
| Client SQL execution, request gate and live WAL publication | SQLite service/commit-barrier VFS |

The old application history/manifest must stop deciding V2 transactions.
Provider checkpoint or candidate files may cache materialized state, but
cannot independently decide whether a transaction belongs to selected history.
There is one V2 recovery log, not a new V2 log beneath the old frame log.

## Target Composition

```text
SQL thread + commit-barrier VFS
             |
complete WalFrameSet -> AtomicRedoOperation
             |
common transaction API -> loggingreplicator managers
             |                         |
private early-LSN V1 bridge       State Manager
             |                         |
transport quorum                 SQLite StateProvider2
                                       |
                         provider images / materialization
```

The service receives one V1-compatible lifecycle bundle for the host and a
transactional handle to that same composition. It does not assemble a second
transport or directly drive the V2 dispatcher from SQL callbacks.

Ported State Manager recovers or creates the SQLite provider through its
factory descriptor. Bootstrap provider creation uses the normal transactional
registry path under admitted primary authority before SQL serving; recovery
and copy reconstruct the same persistent ID. One database per service does
not justify bypassing provider registration or replacing it with a fixed
engine configuration entry.

## Provider Contract

### Lifecycle and identity

Implement naming/factory reconstruction, initialize/open, role changes,
close/abort, prepare-for-remove and local state removal. Provider identity is
a persistent ID plus descriptor, not just a database path.

The host/agent selects role and authority. Provider readiness cannot reopen
SQL after authority loss. Shutdown and copy replacement must close SQL
connections before changing files; asynchronous checkpoint/copy tasks remain
under the creation's writer reservation until contained.

### Apply and resource ownership

Apply receives operation LSN, transaction/commit sequence, role/kind,
metadata/data and returns an optional owned context. For atomic redo the
operation and commit LSN may coincide; do not generalize that equality to
all engine transactions.

Validate `WalFrameSet` geometry, payload identity, page data and final size.
Primary, secondary and recovery materialization must represent the same
operation sequence, but may use different resources.

**Apply can run before primary replication completion or global stability.**
A failed own replication result can occur after provider Apply. Therefore
Apply must not publish the primary's live SQL WAL or release its serving gate
as though it were a commit-success notification.

Original operation contexts, Apply-returned contexts and transaction locks
follow the ported ownership rules. Unlock releases resources; it is not proof
of success, can occur during failure cleanup, and can occur without Apply.
Do not write the SQL commit frame from Unlock.

### Isolation and materialized views

Use a provider-owned candidate materialization/page overlay or equivalent
private view for primary Apply. It derives from the V2 log and is not another
transaction journal. The service's SQL request/publication gate remains held
until the atomic result and local publication are known.

Initially secondaries do not expose client SQL. They may materialize locally
applied atomic records before stability while the framework's read/copy/role
gates remain in force. On promotion, validate/recover the selected history
before opening a serving connection. A numerically higher local image is not
automatically the authoritative database.

The provider must state which lock/version protects checkpoint capture and
source copy from a concurrently changing materialization. Merely routing
callbacks through one provider ID is not isolation.

## Primary Transaction Path

1. Execute SQL on the primary. The VFS buffers the complete transaction
   through its final commit frame and captures one deterministic `WalFrameSet`.
2. Submit that payload as one atomic redo operation through the ported
   transactional API.
3. The ported managers start V1 replication, obtain an early LSN, and append
   the record to the V2 physical log. Replication and local flush/dispatch are
   independent dependencies.
4. Local flushed processing invokes provider Apply. It validates/stages the
   candidate version using resources independent of the blocked SQL
   connection.
5. The transaction path joins Apply and own replication, invokes engine
   resource release, and returns its atomic result. There is no extra stable
   watermark wait.
6. Only on successful completion and current serving authority does the SQL
   thread publish its already captured WAL frames.
7. Confirm local publication before SQL success and release the service gate.

```text
                       +-> V1 replication completion -----+
capture -> submit/LSN -+                                  +-> atomic result
                       +-> log flush -> provider Apply ----+        |
                                                                  |
                                              successful + current authority
                                                                  |
                                                 SQL thread publishes WAL
                                                                  |
                                                        SQL success
```

The VFS thread holds the live connection while awaiting V2. Primary Apply
must not reacquire that connection, perform recursive SQL, or wait for local
WAL publication: any of those would deadlock the completion it is meant to
satisfy. Unlock also cannot wait for the VFS to resume.

This is a provider adaptation to SF's processing model, not a rewrite of
`OperationProcessor` to postpone Apply until quorum. The private early-LSN,
stream and ACK bridge belongs to the runtime; SQLite does not emulate it by
blocking around public `StateReplicator::replicate`.

## Failure and Client Outcomes

| Boundary | Required SQLite behavior |
|---|---|
| Rejection before submission | Do not publish captured WAL; ordinary local rollback remains possible |
| Apply completed, own replication fails or outcome is interrupted | Do not publish or serve the candidate; retain framework recovery evidence and fence the instance as required |
| Own replication succeeds, local Apply fails | No SQL success; fault/fence and recover through V2 |
| Atomic completion succeeds, live WAL publication fails | No rollback claim; fence, preserve V2 log and rematerialize selected history |
| WAL publication succeeds, response is lost | Client outcome is ambiguous; do not execute a retry as exactly-once without an application request ledger |
| Authority changes at any asynchronous boundary | Old tasks cannot publish or reopen SQL for a newer generation |

An error does not by itself prove that an atomic record will never belong to
selected history. The port's history/copy/recovery rules decide that outcome.
Do not delete an applied redo record or invent a durable “aborted SQL” marker
outside the transaction log to settle ambiguity.

The SQL gate and current-generation state protect against a late Apply/Unlock
callback after failure. Reopening is a lifecycle/recovery action, not merely
clearing a Boolean when a future completes.

## Secondary ACK and Recovery

Secondary durable ACK means V2 log flush, with optional commit-classified
Apply acknowledgement as defined by the shared port. It does not inherently
mean SQLite Unlock or client-read publication. SQLite must not request a
global engine rule that delays all secondary Apply until a later committed
watermark.

Recovery is framework-driven:

1. With access closed, validate storage identity and V2 format.
2. Recover State Manager/provider checkpoint state, completing interrupted
   checkpoint publication if required.
3. Reconstruct transaction/log history and replay using the port's
   recovered/copied checkpoint cutoff.
4. Apply self-committing atomic redo in the local recovered history. Its
   presence is not a new authorization to serve SQL.
5. Complete agent-authorized role/history/copy reconciliation.
6. If local redo-only history is false progress, perform the selected full
   copy before opening a serving database.
7. Materialize/validate the selected provider image and open SQL only when
   both framework readiness and partition access permit it.

Do not filter log replay using the old application's applied/committed
manifest or a newly invented V2 per-operation quorum-commit marker.
Recovery can reconstruct candidate local state later replaced by full copy.

Redo must be replay-safe from its declared checkpoint: use a fresh
materialized generation or exact provider operation evidence. Crash after
mutation but before local progress persistence must not apply a logical
increment twice. Validate page contents/geometry and database state; temporary
filenames alone are not durable completion evidence.

## Checkpoints and Retention

Implement the native-equivalent phases under the ported checkpoint managers:

| Phase | SQLite action |
|---|---|
| Prepare | Synchronously capture/pin the database version at the supplied checkpoint LSN and dispatch boundary; it may not yet be stable |
| Perform | After the framework's stability gate, write/flush the captured immutable image while preserving its exact version |
| Complete | Retry-safe promotion/cleanup coordinated with framework end/complete-checkpoint records and copy-log publication |
| Recover | Reopen the selected image; tolerate Complete on a reconstructed instance before Recover without same-instance Prepare/Perform |

Do not substitute the live connection's newest database for the captured
version. If it cannot provide the required snapshot concurrently, the provider
must retain a separate immutable view or explicitly synchronize capture,
without changing the engine's checkpoint boundary.

SQLite automatic WAL maintenance remains separate from V2 checkpoint
authority. Preserve the checkpoint/log suffix needed for recovery,
copy/backup readers, pending/unstable framework transactions and active
versions. Do not truncate solely because the live database has flushed or a
single SQL transaction returned success.

## Copy and False Progress

Use the port's progress-vector/context exchange and none/partial/full mode
selection. SQLite does not independently choose its suffix from numeric LSNs.

Atomic redo-only has no provider undo path. If correcting the target history
would cross an applied/recovered SQLite atomic redo record, preserve SF's full
copy fallback. Partial copy is still valid for compatible histories that do
not require unsupported rollback; SQLite's use of redo-only does not make
every build full-copy-only.

For full copy:

1. Enumerate the framework-selected immutable provider state and retain its
   snapshot resources.
2. Install numbered records into a target staging generation. Exact build
   retries verify identity and bytes.
3. Transfer/replay the framework-selected log range and checkpoint/history
   metadata; do not replace it with an application-selected suffix.
4. Complete provider installation/checkpoint phases and copy-log publication
   under V2 ordering.
5. Finish retained catch-up and independent agent admission before SQL serving.

Begin/Set/End is not a guaranteed cleanup pair. Interrupted copy need not call
End, and an existing provider instance can be reused. Explicit staging
ownership, recovery and Close/Abort must release incomplete resources without
altering a newer selected generation.

Never attempt false-progress repair against a live serving primary
connection, synthesize SQLite undo bytes for a redo-only operation, or merely
truncate the log while retaining a divergent serving database.

## Backup and Restore

Provider BackupCheckpoint copies its selected image. Provider
RestoreCheckpoint installs an image but does not itself perform recovery.
The framework separately handles registry, history, retained log range,
lineage and the subsequent provider/log recovery.

Full and incremental backup follow the ported protocol. A successful
destination callback alone is not complete backup success: V2 still processes
its replicated backup completion record/barrier. Incremental means a
framework-validated log continuation, not arbitrary changes to live SQLite
files.

Restore is offline for SQL and permitted only in the authorized data-loss
window. Validate chain/schema, restore the provider image and framework log,
recover, preserve data-loss epoch rules and remove in-progress restore
evidence only at the correct terminal boundary. A backup directory is not
authority to replace a current replica.

## Migration and Activation

Initially require fresh V2 storage, consistent with the shared design.
Conversion of old base/history state is separately scoped; do not infer V2
progress from `state-v2.json`, a live database, or partially converted files.

| Phase | Deliverable |
|---|---|
| Provider fixture | Deterministic frame provider using the port's real transaction/Apply/Unlock semantics; no production SQL serving |
| Inactive integration | VFS submits atomic redo, independent primary staging, correct publication gate, V2-only checkpoint/recovery/copy |
| Fault qualification | Apply-before-quorum, failure/Unlock races, redo-only full copy, alias cutover and provider image replay tests |
| Activation | Complete faithful-port integration, fresh-format admission and real restart/build/failover/swap evidence |
| Old storage retirement | Stop old frame-log authority; reclaim only explicit obsolete files after the new storage lifecycle is established |

The old complete V1 stack can remain usable for separate test deployments
while V2 is developed. No serving instance mixes its old log with new V2
decisions. After a V2-backed write, never fall back to old history, recovery
or copy.

## Verification and Invariants

Port engine tests separately; SQLite tests verify its provider contract:

| Scenario | Required observation |
|---|---|
| Apply before own replication finishes | Candidate staging succeeds without SQL publication or connection deadlock |
| Replication failure after Apply | No SQL success; Unlock releases resources without publishing WAL |
| Completion while stable LSN lags | SQL may finish after valid completion/publication; no unintended stable wait |
| Crash after V2 completion before WAL publish | Recovery/copy reconstructs the selected V2 history, not the old frame log |
| Publication/response failure | Conservative client outcome and fenced/recoverable local materialization |
| Recovery of divergent atomic redo | Full-copy fallback, no fabricated provider undo and no pre-repair SQL serving |
| Compatible log-only copy | Correct image through framework-selected suffix, including quiet builds |
| Checkpoint Prepare before stability | Captured image is exact; Perform waits framework gate; restart Complete is safe |
| Copy interruption/provider reuse | No guaranteed End dependency; partial staging remains unserved |
| Callback after replacement/Abort | Cannot publish into, delete, or reopen a newer generation |
| Backup callback succeeds but final marker fails | Do not report completed backup prematurely |
| Old/unknown storage | Reject before mutation; no mixed authority or implicit migration |

SQL success requires quorum-backed atomic completion, current authority, and
local WAL publication. Provider Apply, Unlock, physical flush, stable barrier,
and SQL visibility remain distinct milestones. Engine recovery and normal
application must yield equivalent selected database state; no secondary,
copied, or recovered candidate becomes client-visible merely because its
files exist.

## Limitations and References

The first SQLite consumer can remain one database, one provider and atomic
redo-only, with no secondary SQL endpoint. Cross-provider SQL atomicity,
online SQL restore, attached databases and extension/VFS compatibility require
separate application contracts. They do not justify weakening the shared
engine's SF fidelity.

- [Current SQLite design](design.md)
- [Kuberic faithful V2 port design](../kuberic/v2-transactional-replicator-design.md)
- [SF V2 source study](../../background/service-fabric/v2-transactional-replicator.md)
- [Replicator boundary](../kuberic/replicator-boundary.md)
- [`SqliteService`](../../../examples/sqlite/src/service.rs)
- [`SqlitePersistence`](../../../examples/sqlite/src/state.rs)
- [`DurableFrameLog`](../../../examples/sqlite/src/framelog/durable.rs)
- [Commit-barrier VFS](../../../examples/sqlite/src/commit_barrier/mod.rs)
