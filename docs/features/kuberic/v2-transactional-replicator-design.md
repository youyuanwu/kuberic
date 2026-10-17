# Kuberic V2 Transactional Replicator Design

## Status and Direction

Proposed architecture, not an implemented feature. Research baselines:
Kuberic `9eaac46cac5fa674f17c2082918186a9bafeef67` and native Service Fabric
`3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d`, inspected on 2026-10-16.

**Implement a faithful, idiomatic Rust port of SF V2 over Kuberic's existing
V1 infrastructure. Preserve SF's component boundaries, recognizable source
layout, state machines, processing order, and recovery rules. Use Rust async
and ownership to simplify implementation mechanics, not to redesign the
protocol.**

This supersedes the earlier proposal for a simplified committed-only
atomic-redo engine. In particular, there is no replacement policy of
quorum-then-provider-apply, one immutable provider, full-copy-only recovery,
or a new per-operation durable commit marker driving provider application.
An atomic-redo test slice is an implementation milestone, not the target
engine contract.

The evidence and qualifications are in the
[SF V2 source study](../../background/service-fabric/v2-transactional-replicator.md).
[SQLite integration](../sqlite/v2-transactional-replicator.md) describes an
application provider, not a different transactional engine. Here V2 means the
SF transactional layer, not Kuberic's existing level-triggered v2 host.

## Port Contract

1. Port native SF V2 transaction, logging, State Manager, checkpoint, recovery,
   copy, version-retention, and backup/restore behavior. Native code is the
   primary baseline; do not combine native and managed signatures silently.
2. Preserve the SF directory/component organization and recognizable class
   responsibilities. Translate names and implementation patterns into Rust
   without collapsing the managers into a new generic coordinator.
3. Replace KTL, native allocation/async machinery, platform logging, tracing,
   and hosting integrations with explicit Rust/Kuberic equivalents.
4. Retain one V1 transport/reconfiguration engine. Add private integration
   seams rather than a competing topology protocol or second built-in engine.
5. Do not change `Replicator`, `PrimaryReplicator`, or `StateProvider`.
   Keep existing V1 applications and their public completion contracts intact.
6. Keep one durable transaction/recovery authority: the ported V2 log plus
   provider checkpoints. Agent authority remains independently agent-owned.
7. Port relevant SF tests alongside each component. Track every semantic
   deviation and source defect separately; fidelity is not blind reproduction
   of known mistakes.

Target capabilities include regular multi-operation/multi-provider
transactions, undo-bearing atomics, redo-only atomics, transactional provider
add/remove and hierarchy, barriers, snapshot-version retention, all SF copy
modes, full/incremental backup, and data-loss-authorized restore. Unfinished
capabilities return explicit unsupported errors or remain unexposed; they do
not become permanent simplifications of the design.

Non-goals are C++/KTL embedding, SF wire/file compatibility, SF cluster
management, a port of Reliable Collections/TStore in this project, distributed
cross-partition transactions, and transparent client exactly-once execution.

## Component and Source Layout

Keep the native `txnreplicator` organization, with Rust file names:

```text
kuberic-runtime/src/
  txnreplicator/
    mod.rs
    transactional_replicator.rs
    transactional_replicator_factory.rs
    kuberic_bridge.rs
    common/
      mod.rs
      transaction.rs
      transaction_base.rs
      transaction_state_machine.rs
      atomic_operation.rs
      atomic_redo_operation.rs
      transactional_replicator.rs
      transaction_manager.rs
      state_provider2.rs
      state_provider_map.rs
      version_manager.rs
      operation_context.rs
      apply_context.rs
    logrecordlib/
      mod.rs
      log_record.rs
      transaction_log_record.rs
      operation_log_record.rs
      progress_vector.rs
      copy_mode_result.rs
      ...
    loggingreplicator/
      mod.rs
      logging_replicator.rs
      replicated_log_manager.rs
      physical_log_writer.rs
      transaction_manager.rs
      transaction_map.rs
      operation_processor.rs
      log_records_dispatcher.rs
      parallel_log_records_dispatcher.rs
      checkpoint_manager.rs
      recovery_manager.rs
      copy_stream.rs
      secondary_drain_manager.rs
      state_replicator.rs
      truncate_tail_manager.rs
      version_manager.rs
      backup_manager.rs
      ...
    statemanager/
      mod.rs
      state_manager.rs
      state_provider2_factory.rs
      metadata_manager.rs
      checkpoint_manager.rs
      api_dispatcher.rs
      named_operation_data_stream.rs
      ...
    testcommon/
      mod.rs
      ...
  logicallog/
    mod.rs
    ...
```

This is a component map, not an exhaustive file list. Add the corresponding
record/manager files as they are ported, preserving upstream ownership. The
two checkpoint managers remain distinct: logging owns log protocol and stable
boundaries; State Manager owns provider metadata/checkpoint fan-out.
`logicallog` mirrors the lower logging dependency, not a replacement
transaction manager. `kuberic_bridge` is an explicit Kuberic-only boundary.

Rust traits need not retain the native `I` prefix or separate declaration
headers, and platform adapters need not reproduce native wrapper classes.
Keep each substantial interface, state machine and manager traceable to its
source component rather than gathering every contract in a single new file.

| SF component | Rust ownership |
|---|---|
| Top-level transactional replicator | Composition, service lifecycle and API forwarding |
| `common` | Transaction/provider/version interfaces, IDs, contexts and transaction state machines |
| `logrecordlib` | Logical/physical record model, links, progress vectors and copy-mode decisions |
| `loggingreplicator` | Replication/log joins, dispatch, transactions, recovery, checkpoints, copy, truncation, versions and backup |
| `statemanager` | Named provider registry, factories, hierarchy, metadata locks, apply routing and provider lifecycle |
| `logicallog` | Async append/read/flush/truncate and crash-recoverable log replacement primitives |
| `testcommon` | Deterministic providers, transport/log faults, phase gates and SF test equivalents |

Expose only intended Rust APIs through reviewed re-exports. Module layout does
not require making implementation managers public. Keep host-dependent
composition gated so controller-only builds do not acquire a host dependency.

Maintain a port inventory mapping each upstream file/type/test to its Rust
counterpart and status. Record source commit, contract coverage, platform
replacement, and any intentional deviation. Do not claim conformance from
matching names alone.

## Idiomatic Rust Without Semantic Drift

| Native mechanism | Rust approach and invariant |
|---|---|
| KTL awaitables/completion tasks | `async fn` plus owned, shareable phase completions; preserve flush/apply/replication/processing as different events |
| Begin/end async plumbing | Ordinary futures and explicit submission handles; retain early LSN versus later completion |
| Shared native objects | `Arc`/`Weak` for shared lifetimes; owned records/ID-based arenas where appropriate |
| Parent/child transaction links | Typed record IDs and explicit graph ownership; preserve traversal order without strong-reference cycles |
| Status codes and output arguments | Typed `Result`, enums and return values; preserve error categories and retry rules |
| Allocation tags and raw buffers | Owned buffers, `Bytes`, bounded queues and checked lengths; no emulated KTL allocator |
| Role/kind bit masks | Typed roles and apply kinds; preserve meaningful combinations and explicit false-progress context |
| Runtime tasks and cancellation | Scoped task ownership, explicit Close/Abort and cancellation tokens; no detached durability work |
| Native locks | Short, documented locks; preserve per-transaction ordering and cross-transaction concurrency |
| Blocking filesystem calls | Async backend or bounded blocking workers; retain exact flush and directory durability requirements |

Use `join!` or an equivalent non-short-circuit join when both apply and
replication must complete. A `try_join!` that drops the remaining future on
first error can change SF's processing/resource-release behavior. Multiple
waiters need a cached phase result with race-safe notification, not a
single-consumer future or a lossy wakeup.

Dropping a Rust future is not a transaction Abort. Once submitted, the owner
retains the operation through its specified completion or containment path.
`Drop` cannot await cleanup: use explicit async disposal/close and an owned
supervisor for abort work, with synchronous fencing before return.

Keep native PrepareCheckpoint synchronous where it captures state at a
serialized boundary. Rust async is not a reason to insert an await in that
capture. Likewise, do not replace the parallel dispatcher with a permanently
serialized actor merely because it is simpler to write.

## Existing Kuberic Boundaries and Required Work

The current engine is useful infrastructure, but not a drop-in implementation
of SF V2's lower interface.

| Existing boundary | Baseline evidence | Required adaptation |
|---|---|---|
| Coherent factory registration | `kuberic-runtime/src/replicator/mod.rs:280-318,981-1029` | One outer lifecycle bundle, one creation identity and one listener |
| Public replication completion | `kuberic-runtime/src/replicator/traits.rs:139-162` | Keep public completion unchanged; supply early-LSN/private completion separately |
| Primary durable apply before quorum | `kuberic-runtime/src/replicator/stateless/mod.rs:2828-2991` | Factor private transport/log-acceptance phases instead of making this whole method V2's scheduler |
| Direct secondary durability delivery | `kuberic-runtime/src/replicator/stateless/mod.rs:1024-1125` | Provide actual V2 operation streams with durability/apply ACK dependencies |
| Provider persistence hooks | `kuberic-runtime/src/engine.rs:19-50` | Retain existing V1 `DurableState`; do not redefine its commit callback as V2 provider Apply |
| Provider epoch/copy/progress | `kuberic-runtime/src/traits.rs:44-117` | Separate protected public adapter semantics from SF's private logging-provider callbacks |
| Host readiness | `kuberic-runtime/src/host/hosting.rs:499-555,917-969` | Add private V2 readiness/progress evidence; equality of two old watermarks is insufficient |
| Existing retained catch-up | `kuberic-runtime/src/replicator/stateless/mod.rs:1346-1547` | Preserve V1 behavior while admitting V2 history-aware readers and copy proofs |

Audit findings in the [SF contract tracker](service-fabric-contract-audit.md)
remain open unless fixed independently with evidence. Access admission, swap
capability/staging, containment, retained-boundary proofs and catch-up behavior
must be qualified before V2 activation.

### Private V1 transport bridge

Port the seam represented by SF
`loggingreplicator/IStateReplicator.h:19-29`, not just its method names.
Its Rust equivalent supplies:

- start replication and return an assigned LSN plus owned completion handle;
- secondary replication and copy streams with explicit ACK/reject;
- target-to-source copy-context exchange;
- maximum message size and bounded admission;
- exact epoch, session, configuration, storage and build fencing.

A Rust `start_replicate(...).await` may await local admission/LSN reservation,
but must not wait for quorum before returning its submission handle. The
caller is `ReplicatedLogManager`, which initiates replication, receives the
LSN, appends locally, and later joins the independent completions.

V1 counts local durability only when the V2 physical log has successfully
flushed the relevant record; reservation or delivery alone is not an ACK.
Secondaries ACK according to the ported flush/optional commit-apply policy.
Do not block an executor thread waiting for a public `replicate()` call to
manufacture an early LSN, and do not count an unflushed local record toward
quorum.

Private refactoring must remove circular waits between LSN allocation, V2
append/flush, V1 local acceptance, and quorum completion. V2 owns log
durability and provider dispatch; V1 owns transport quorum. Avoid two
independent per-operation logs or duplicate provider application.

V2's physical log is not the existing one-operation-per-LSN retained iterator.
Preserve replicated payloads/LSNs separately from physical records and
same-LSN epoch records; copy/recovery readers retain the latter in their
correct positions. Do not manufacture filler operations or increment a
logical LSN for an epoch record just to satisfy the old iterator.

Existing V1 applications continue using their existing durability path and
public `StateReplicator`. This is explicit storage-layer composition through
one transport, not a resurrected legacy engine or hidden fallback.

### Host and public progress bridge

Do not copy SF's V1-facing “last committed” name into Kuberic and return the
logical tail as if it proved quorum commitment. The source study records
that native naming difference.

Keep a private V2 progress snapshot carrying history, logical/durable tail,
transport quorum evidence, stable LSN, visibility barrier, recovered/copied
checkpoint, processing readiness and fault/copy state.

Public `Replicator::current_progress` keeps its existing primary/secondary/
recovery meaning. Public `StateProvider::last_committed_lsn` reports the
committed log progress its contract requires, not the last user transaction
commit, provider-visible version, or an unflushed tail. The private SF logging
provider can separately expose its native-equivalent tail to the inner
transport. These adapters must not be confused.

Public progress that requires durable quorum evidence must be backed by
history-bound Kuberic transport/agent evidence or fresh authorized
reconciliation. Reopening an SF atomic/end record alone cannot reconstruct
that proof. Keep required adapter progress evidence distinct from V2
transaction decisions; it must not become a second transaction log or gate
ported provider Apply. If the proof is unavailable, remain access-closed and
report pending/rebuild through the appropriate host path rather than
inventing a committed tail. Private host/schema changes needed for this
bridge are part of the port, not changes to the protected interfaces.

The same separation applies to copy: the protected public
`StateProvider::get_copy_state` retains its frozen committed upper-bound
contract. The private logging-provider copy protocol carries SF's richer
history/checkpoint metadata; do not silently reinterpret the public callback
as permission to transfer beyond its supplied boundary.

Likewise, carry the authorized previous-epoch committed boundary and the
private SF previous-tail boundary separately. Validate that both belong to
the exact history before invoking the port's epoch transition; do not replace
one with the other because both are integers.

Host serving requires current agent authority, V2 recovery/role/copy/read
consistency, and provider/application readiness. Ordinary buffered transaction
members must not force permanent access denial merely because their log tail
is ahead of a visibility barrier. Conversely, equal transport watermarks
cannot reopen access during failed provider recovery or SQLite publication.
Implement this as explicit private V2 admission evidence, not dummy zero
progress or changed protected signatures.

## API and Provider Semantics

Port the responsibilities of native `ITransactionalReplicator`,
`ITransactionManager`, `IStateProviderMap`, `IVersionManager`,
`IStateProvider2`, and the provider factory as Rust interfaces. `StateProvider2`
and its factory are separate from protected V1 `StateProvider`.

| API area | Target behavior |
|---|---|
| Regular transaction | Buffered first operation; begin/member/end records, single-operation optimization, commit/abort, disposal and visibility registration |
| Atomic variants | One operation per object; separate undo+redo and redo-only contracts |
| Provider map | Transactional add/remove/get-or-add, metadata locks, persistent IDs, hierarchy, enumeration and tombstone retention |
| Provider Apply | Own operation LSN, transaction/commit sequence, role/kind, metadata and redo/undo; nullable owned context |
| Unlock | Route original/Apply-returned contexts correctly; support abort cleanup without prior Apply |
| Checkpoint | Synchronous prepare capture, async perform/complete/recover, restartable completion |
| Copy | Source streams and target Begin/numbered Set/End, including interrupted-copy cleanup |
| Version services | Register/unregister snapshot visibility and defer provider-version/checkpoint deletion |
| Backup/restore | Provider checkpoint callbacks plus framework log-chain/authorization protocol |

Preserve the native retry state machine and distinguish native versus managed
defaults. Native commit cancellation/timeout stops retries after a retriable
failure; it is not an unconditional interruption or automatic rollback.
Use Rust error types and explicit outcomes without claiming stronger
definitely-aborted or exactly-once guarantees.

Provider isolation remains a provider responsibility. State Manager dispatch
and a shared transaction ID do not automatically make arbitrary provider
reads serializable. Port transaction resource tracking and version services;
test multi-provider visibility with providers implementing the required
locking/version contracts. Do not substitute a new global publish-all
protocol for SF's provider contract.

## Records and Sequence Domains

Port SF's logical and physical record families, including begin/member/end,
atomic redo/undo, barriers, epochs, backup, checkpoint/index/truncation and
information records. Preserve parent/child and physical checkpoint links,
ordering rules, recovery offsets, and epoch records sharing a logical LSN.

| Domain | Meaning |
|---|---|
| Transaction ID | Transaction identity, independent of log ordering |
| Operation LSN | V1-assigned logical record sequence |
| Logical tail | Highest appended logical position; not automatically flushed or quorum committed |
| Durable tail | Locally flushed record boundary |
| Transaction commit LSN | End-record LSN or single-operation/atomic record LSN |
| Transport quorum completion | Completion of replication under exact admitted PC/CC authority |
| Stable LSN | Ported barrier-derived stability, used for history/checkpoint/truncation decisions |
| Visibility version | Applied/registered barrier version used by snapshot readers |
| Checkpoint LSN | Recovery cutoff and captured provider state boundary |
| PSN / physical position | Local record order and byte position, including physical records |

There is no universal `applied <= committed` arithmetic across these domains.
Provider Apply may precede the own replication result, and a transaction may
complete before its commit LSN is stable. Phase events and history identity
are as important as numeric watermarks.

Replace native serialization mechanics with checked Rust encoders while
retaining logical fields and record relationships. SF binary compatibility is
not required. Define a versioned Kuberic format and reject unknown required
features before writable setup; changing encoding is not permission to drop
pending-transaction or progress-vector metadata.

## Primary Processing

```text
Transaction / State Manager
        |
TransactionManager -> ReplicatedLogManager
        |
start V1 replication -> assigned LSN -> replication completion -------+
        |                                                           |
ordered append -> physical flush -> dispatcher -> provider Apply ----+
                                                                    |
                                                  join both results
                                                                    |
                                                      invoke Unlock
                                                                    |
                                                   report completion
```

Port the ordered-append queue for concurrently assigned LSNs and group-commit/
barrier scheduling. Provider redo is driven by a committed end record for
ordinary multi-operation transactions, or the self-committing begin/atomic
record. Pending ordinary members are not independently redone.

Preserve these completion facts:

- Primary Apply can run before own replication succeeds.
- The transaction waiter joins apply and replication, then invokes engine
  Unlock; it does not add a separate stable-LSN wait.
- A replication failure after Apply is not evidence Apply never happened.
- Original operation resources, Apply-returned resources and separately
  registered transaction locks have distinct ownership/release paths.
- Unlock error/fault handling and returned commit status follow the reviewed
  native boundary unless an explicit deviation is approved. A void/faulting
  cleanup path does not imply successful cleanup is part of the return value.

Sources: SF `loggingreplicator/TransactionManager.cpp:236-526`,
`loggingreplicator/ReplicatedLogManager.cpp:382-396,781-845,915-957`, and
`loggingreplicator/OperationProcessor.cpp:333-356,402-615,665-915`, under the
source-study root.

## Secondary Dispatch, ACK, and Barriers

Port `SecondaryDrainManager` rather than translating incoming operations into
a new quorum-committed-only callback queue.

1. Obtain the V1 record and metadata LSN; deserialize and append locally.
2. Flush the physical log before successful replication ACK.
3. In commit-apply acknowledgement mode, additionally await successful Apply
   for commit-classified records before ACK.
4. Run the dispatcher and resource release with their own completion phases.
   Neither ACK mode automatically promises Unlock completion.

Ordinary transaction members wait for their committed end before provider
redo. A flushed committed end or atomic record can nevertheless be applied
before global stability. The provider's isolation/read-version rules and
secondary read-consistency gate protect serving; do not globally delay all
Apply calls until a later transport watermark.

Port parallel dispatch across independent transaction chains while preserving
per-chain order and dispatch boundaries. A commit-apply-ACK configuration must
also preserve the dispatch classification that allows its commit to run
without waiting for an ACK-dependent future barrier.

Port barrier creation and stable propagation exactly: the primary advances to
the successfully processed/replicated barrier's LSN; a secondary uses the
stable LSN carried by the barrier. Idle progress still needs a later barrier
when required. Stable transaction-map cleanup is not provider Unlock.

Sources: SF `loggingreplicator/SecondaryDrainManager.cpp:805-971`,
`loggingreplicator/ParallelLogRecordsDispatcher.cpp:119-150,191-246,303-396`,
`loggingreplicator/CheckpointManager.cpp:812-829,1144-1219,1554-1621`, and
`logrecordlib/LogRecord.cpp:127-147,396-415`.

## Logical Log and Platform Replacement

Provide a Rust `logicallog` abstraction implementing the guarantees consumed
by the port: ordered append/read, flush-with-boundary evidence, tail/head
truncation, physical-reader pins and recoverable copy/current/backup log
replacement. A file-backed implementation can replace KTL without porting its
allocator or shared-log infrastructure.

Preserve the split between `ReplicatedLogManager`, `PhysicalLogWriter`,
`LogManager` and the backend. Do not replace them with a new central commit
watermark database. Backend metadata can select generations/aliases, but
transaction decisions remain in ported logical records and stability in the
ported barrier/checkpoint protocol.

Specify and fault-test record framing, length checks/checksums, torn-tail
classification, file flush, atomic replacement and parent-directory flush on
supported filesystems. Do not infer arbitrary-corruption salvage from the SF
reader. Missing/corrupt acknowledged history fails closed; generation
selection must not expose a mixture of provider and log state.

Reader pins, checkpoint dependencies and truncation completion survive their
required lifetime. Disk pressure, failed fsync, copy-log replacement failure
and cancellation surface as explicit faults/errors, not empty-log fallbacks.

## State Manager and Version Retention

Port provider metadata as replicated state: persistent IDs distinct from
names, transactional provisional create/delete, factory descriptors,
parent/child reconstruction, delayed-delete/false-progress metadata and
safe-LSN cleanup. Do not replace it with a configuration file containing one
fixed provider.

Port metadata lock contexts and data-provider resource routing. Abort/dispose
must release a buffered first operation even if it was never replicated or
Applied. Provider removal must exclude conflicting data operations and delay
physical cleanup while checkpoint/copy/replay can still reference the ID.

Snapshot transactions register a visibility version and release it through
the ported lifetime rules. Keep duplicate registration counts and version
interval/removal notifications. Physical log readers, pending transactions,
completed-but-unstable chains and provider snapshot versions are independent
retention mechanisms, not one “GC below commit” rule.

Sources: SF `statemanager/StateManager.cpp:707-858,2796-2859,4305-4451,5294-5418`,
`statemanager/MetadataManager.cpp:928-1076`,
`common/Transaction.cpp:260-280,338-403`, and
`loggingreplicator/VersionManager.cpp:133-350`.

## Checkpoint and Recovery Protocol

Port both checkpoint managers and their linkage to the physical log.

| Phase | Required behavior |
|---|---|
| Prepare | Capture provider/metadata version at the dispatch boundary; can occur before that checkpoint LSN is stable |
| Perform | Wait for checkpoint stability, then persist the captured provider/metadata set without substituting later live membership |
| Complete | Write/flushed end-checkpoint, complete provider/metadata publication, write/flushed complete-checkpoint; coordinate copy-log publication where applicable |
| Restart | Complete an interrupted checkpoint on reconstructed instances, possibly before provider Recover and without same-instance Prepare/Perform |

Do not require every checkpoint to drain/abort all open transactions. Port
the earliest pending-transaction boundary, transaction map and retention needed
to recover transactions spanning the checkpoint.

Recovery selects the checkpoint through physical links, scans from its
earliest pending boundary, rebuilds transaction/history state, and applies
redo based on **transaction commit sequence** relative to the recovered/copied
checkpoint. It drains processing before exposing readiness and aborts
inherited unfinished transactions on primary establishment.

Recovery can reconstruct locally committed state later identified as false
progress. Do not filter replay using a newly invented local quorum-commit
marker. Keep serving closed until agent authority, history correction,
role/copy drain and provider read consistency are established.

Sources: SF `loggingreplicator/CheckpointManager.cpp:368-448,963-975,1466-1549`,
`loggingreplicator/RecoveryManager.cpp:117-345,374-648`,
`loggingreplicator/OperationProcessor.cpp:776-903`, and
`statemanager/StateManager.cpp:2982-3057`.

## Copy and False Progress

Port progress-vector comparison and all four results: none, partial/log,
false-progress correction plus partial, and full copy. Implement a real
target-to-source context exchange in the private V1 bridge; current
source-local context is not sufficient.

For undo-capable histories, port reverse traversal and provider
`SecondaryFalseProgress` application, transaction-map repair, progress-vector
truncation, physical/logical tail truncation, and read-consistency reset.
Respect stable/checkpoint floors and in-progress head-truncation constraints.

For redo-only divergence, preserve SF's no-undo rule: if rollback would cross
the last recovered atomic redo operation, choose full copy. Full replacement
is not the generic substitute for undo across every provider.

Full copy transfers registry/provider state and required logical log history,
possibly starting before the checkpoint for an open transaction. Begin/Set/End
is a success-path protocol; End is not guaranteed on interruption. Retain
provider instance reuse and cleanup semantics, numbered records, staging log,
first-checkpoint phases, exact cutover boundary, alias publication and
post-copy drain. Finish copy drain before consuming the replication stream.

SF can raise a requested copy upper LSN to satisfy its selected checkpoint's
stable boundary. Preflight that effective boundary before persisting Kuberic's
exact frozen build authority. If selection later requires another boundary,
obtain matching fresh authority before exposing bytes; do not raise a
persisted authorized boundary inside a retry.

Kuberic build identity, storage incarnation and stale-task fencing wrap that
protocol. An exact persisted chunk retry still verifies identical bytes.
If an SF streaming enumeration cannot reproduce a Kuberic retry, restart the
authorized build or retain a frozen transfer artifact; never reuse its build
identity for different bytes.

The old V1 retained-boundary proof requirement cannot force fabricated V2
operations. Port a history/checkpoint-aware private proof for the V2 transport
path, or retain the exact required record until that proof exists. Keep the
existing V1 proof behavior unchanged and test quiet builds explicitly.

Sources: SF `logrecordlib/ProgressVector.cpp:131-257`,
`logrecordlib/CopyModeResult.cpp:75-93`,
`loggingreplicator/TruncateTailManager.cpp:84-99,213-448,760-787`,
`loggingreplicator/CopyStream.cpp:249-445`, and
`loggingreplicator/SecondaryDrainManager.cpp:185-235,415-433,998-1099,1214-1268`.

## Backup, Restore, and Lifecycle

Port full/incremental backup as framework operations, not just provider image
copies. Preserve checkpoint/log consistency locks, reader pins, lineage,
epoch/progress-vector eligibility, destination callback and the subsequent
replicated backup completion record/barrier. Callback acceptance alone is not
complete backup success.

Restore runs within agent-authorized data-loss handling and the ported restore
permission window. Preserve chain verification, Safe/Force distinction,
in-progress restore evidence, provider checkpoint restore followed separately
by recovery, data-loss epoch processing and interrupted-restore cleanup.

Port role-specific ordering: inner V1 role work before V2, logging before
provider primary/idle transitions, copy drain before idle-to-active, dispatch
coordination around provider active transition, and new-primary barrier/
pending-transaction handling. Keep Close/Abort's full resource scope.

Create one coherent factory bundle plus a transactional handle to the same
allocation. Preserve creation identity, capability registration, callback
containment, settings and one listener. The provider-writer reservation must
cover V2 checkpoint/copy/backup descendants even after the inner engine starts
closing; no replacement writer can overlap them.

Sources: SF `loggingreplicator/BackupManager.cpp:285-406,430-555,707-784,1477-1582,1840-1904`,
`TransactionalReplicator.cpp:257-330,402-442`, and
`loggingreplicator/LoggingReplicatorImpl.cpp:891-916,1136-1152,1434-1467`.

## SQLite Consumer

SQLite may use one provider and one atomic redo operation per complete
`WalFrameSet` without limiting the engine to that subset. Keep WAL capture,
page interpretation, provider snapshots and local SQL publication in the
SQLite provider; remove its old frame log as a competing transaction authority.

The provider must handle SF Apply before own replication completion, resource
release on failed completion, and redo-only false-progress full copy.
Secondary/recovery Apply may materialize candidate local history while SQL
access is closed; provider mutation is not permission to serve it.

The primary VFS waits on V2 from the thread holding the live SQLite
connection. Primary Apply must validate/stage via independent resources and
return without acquiring that connection. A provider operation context and
SQL request gate prevent premature publication. Only successful atomic
completion permits the SQL thread to publish its captured WAL; SQL success
also waits for that publication.

Do not rely on Unlock alone as proof of success: failure cleanup also invokes
it. Publication failure or an indeterminate replication result fences the SQL
instance until authoritative recovery/copy. The
[SQLite proposal](../sqlite/v2-transactional-replicator.md) defines these
provider-specific states and tests.

## Fidelity, Deviation, and Licensing Rules

For every ported component, distinguish contract intent, observed native
ordering and known implementation anomalies. Preserve successful-path
semantics and necessary failure/resource ordering; do not silently reproduce
or “fix” ambiguous behavior.

The source study identifies atomic retry-state, copied-record ACK error and
backup-status anomalies. Each needs a recorded decision, focused regression
and rationale before its Rust equivalent is accepted. Defaults/callback
concurrency not established by research remain explicit questions, not guessed
behavior. Rust `Result` must surface errors through the repository fault path,
not convert them into success-shaped defaults.

Retain upstream test names/scenarios in Rust equivalents or a traceability
map. Platform/representation changes are expected; changing checkpoint,
stability, undo or commit semantics requires an explicit deviation.

SF is MIT-licensed. When implementation is ported, preserve required upstream
copyright/license attribution in the separate notice mechanism, extend
`THIRD-PARTY-NOTICES` with precise provenance, and leave the original root
`LICENSE` unchanged. No SF implementation code is copied by this design.

## Implementation Sequence and Activation

| Phase | Deliverable and exit condition |
|---|---|
| 1. Port inventory and skeleton | Matching component layout, native contract map, Rust type/error/lifetime rules, source/test traceability and deviation ledger |
| 2. Platform and V1 bridge | Logical-log backend, early-LSN/owned completion, actual streams/ACKs, target copy context, progress/authority mapping and lifecycle containment |
| 3. Records and transaction core | Record families, ordered append, writer/dispatcher, regular and both atomic forms, transaction map, Apply/Unlock joins and barrier stability; SF scenario tests |
| 4. State Manager and versions | Dynamic provider map/hierarchy, locks/contexts/tombstones, version registration/removal and multi-provider test provider |
| 5. Checkpoint and recovery | Both checkpoint managers, spanning transactions, physical-link recovery and interrupted completion; crash/fault matrix |
| 6. Reconfiguration and copy | All history/copy modes, undo and redo-only fallback, copy-log publication, exact build retries and role/drain ordering |
| 7. Backup and restore | Full/incremental chain, completion marker/barrier, data-loss window and interrupted restore |
| 8. SQLite and activation | Provider-specific Apply/publication isolation, format admission, failover/swap/build/restart/crash evidence; no legacy recovery fallback |

Components can be implemented/tested incrementally, including early atomic
fixtures, but incomplete slices are not a new production storage protocol.
Resolve dependencies in this sequence before serving V2-backed application
writes; do not label a reduced permanent MVP as the faithful port.

Initially require fresh V2 storage. Any old-state conversion is a separately
specified offline procedure with exact history and operation evidence. Reject
unknown/mixed V1/V2 formats before mutation. After V2 accepts a write, never
fall back to the old application frame log, old copy, or old recovery.

## Verification Strategy

Use existing Rust/nextest and runtime API guards. Port SF component tests into
the matching modules and keep Kuberic integration tests for authority and
hosting. Add controllable storage/transport/provider phase gates and a
deterministic history model; test interleavings, not just final counters.

| Boundary | Required evidence |
|---|---|
| Early LSN | Returned before quorum and before caller-local append completes; no reservation-only durable vote |
| Apply/replication join | Either can finish first; failed replication after Apply fails completion and releases resources correctly |
| Transaction chain | No ordinary member redo without committed end; all members share commit sequence and retain own operation LSNs |
| Unlock/disposal | Original/returned/null contexts, buffered abort without Apply, snapshot unregister and no double release |
| Commit versus stability | Successful completion while stable LSN lags; no added stable wait or stable-equals-commit shortcut |
| Both secondary ACK modes | Flush before ACK, optional commit Apply before ACK, no implicit Unlock promise or barrier deadlock |
| Dispatcher | Per-transaction order, independent-chain concurrency and epoch/checkpoint/commit dispatch boundaries |
| Checkpoint/recovery | Prepare before stable, delayed Perform, membership changes between phases, spanning transaction and missing complete marker |
| False progress | Reverse undo/map repair above floors; redo-only crossing selects full copy, never fabricated undo |
| Copy | None/partial/full, instance reuse, interrupted Begin/Set without End, exact retry bytes, quiet builds and restart at alias cutover |
| Retention | Pending/unstable chains, physical copy/backup readers and duplicate snapshot versions all block their respective reclamation |
| Backup/restore | Destination callback versus final completion, broken lineage, epoch eligibility, Safe/Force and interrupted restore |
| Hosting | Exact PC/CC/session/storage fencing, status-only revocation, writer exclusivity, no stale callbacks and contained descendants |
| SQLite | No primary connection deadlock or pre-success WAL publication; failure cleanup not treated as commit |
| Format/faults | Corrupt/missing acknowledged history, torn tails, disk-full/fsync failure and rejected formats fail explicitly |

Where a native test uses mocks, add Kuberic tests at the real dependency
boundary; mocks alone do not establish filesystem or distributed durability.
Differential traces or fixtures must compare operation/commit positions,
apply kinds, ACK/Unlock order, history/copy decisions, recovery state and
visible provider contents. Record deliberate deviations in the oracle.

Before activation, validate real failover, planned switchover, full/partial
build and restart with the complete port. Existing V1 application behavior and
protected API inventories remain unchanged. PostgreSQL's native replicator
is independent; routine checks use `just nextest-postgres-smoke` if needed.

## Remaining Bounded Decisions

Settle the concrete Rust log-backend framing and filesystem assumptions,
minimum format features, provider schema evolution and resource/disk limits.
Resolve unverified native defaults and same-provider callback concurrency
from additional source evidence and focused tests before implementing them.

These choices do not reopen the architecture: preserve SF component layout,
transaction/Apply/ACK/stability/undo/recovery semantics and test traceability.
Rust async simplifies the machinery around those semantics, not the
correctness obligations.
