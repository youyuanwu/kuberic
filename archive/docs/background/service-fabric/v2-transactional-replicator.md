# Service Fabric V2 Transactional Replicator

## Scope and Evidence

Source study dated 2026-10-16, using the clean local checkout
`/home/user1/code/reference/service-fabric` at commit
[`3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d`](https://github.com/microsoft/service-fabric/tree/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d).
This document describes existing SF contracts and implementation, not
implemented Kuberic behavior.

The primary subject is the native C++ V2 implementation. Managed interfaces
are identified explicitly where their contracts differ. **Contract** below
means an interface declaration/comment; **implementation** means observed
control flow at this revision. Test references describe inspected assertions,
not tests executed during this research. No SF build, distributed experiment,
or exhaustive correctness audit was performed.

Unless a path starts with `src/`, source citations are relative to
[`src/prod/src/data/txnreplicator/`](https://github.com/microsoft/service-fabric/tree/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/data/txnreplicator)
at that commit. For example, `common/IStateProvider2.h:143-155` is a
repository-relative file and inclusive line range under that directory.
References remain tied to this revision.

> Part of the [SF Architecture Reference](README.md).
> The corresponding proposal is
> [Kuberic V2 Transactional Replicator Design](../../features/kuberic/v2-transactional-replicator-design.md);
> [SQLite integration](../../features/sqlite/v2-transactional-replicator.md)
> is application-specific.

## Executive Summary

V2 adds transactions, a durable log, provider management, recovery,
checkpointing, history-aware copy, version retention, and backup/restore above
the V1 replicator. It does not replace V1's replication and reconfiguration
object. Construction creates a logging callback adapter, the V1 replicator,
and then the V2 logging/state-manager stack around it
(`ComTransactionalReplicator.cpp:183-288`).

The most important semantic distinctions are:

- An ordinary multi-operation transaction applies provider redo at a committed
  end record, not at each received member record. A single-operation
  transaction or atomic operation has a self-committing record instead
  (`loggingreplicator/OperationProcessor.cpp:665-915`).
- Primary completion joins local apply and the operation's V1 replication,
  invokes unlock, and returns. It does **not** explicitly wait for the stable
  LSN to reach the transaction's commit LSN
  (`loggingreplicator/TransactionManager.cpp:498-526`).
- Secondary ACK normally follows successful local flush. A setting adds
  commit-record apply before ACK; neither mode explicitly waits for unlock
  (`loggingreplicator/SecondaryDrainManager.cpp:908-971`).
- A locally processed committed transaction can still be false progress
  relative to the selected primary history. Undo-capable records can be
  corrected; crossing a recovered atomic redo-only record requires full copy
  (`loggingreplicator/TruncateTailManager.cpp:213-321,359-448`;
  `logrecordlib/CopyModeResult.cpp:75-93`).
- Operation LSN, transaction commit LSN, logical tail, stable LSN, snapshot
  visibility version, and physical position are separate quantities. A method
  named “last committed” is not sufficient evidence that they coincide
  (`common/IStateProvider2.h:143-148`;
  `loggingreplicator/LoggingReplicatorImpl.cpp:435-444,577-587`).

## Architecture and Construction

```text
Reliable Collection / custom provider API
                 |
        Transaction + provider locks/versions
                 |
    State Manager + Transaction Manager
                 |
            Logging Replicator
    recovery, checkpoints, log, copy, backup
                 |
     V1 replication + operation streams
                 |
           other replicas
```

| Component | Responsibility and evidence |
|---|---|
| `TransactionalReplicator` | Compose State Manager and logging; coordinate open and role changes (`TransactionalReplicator.cpp:85-121,257-330,402-442`) |
| `TransactionManager` | Construct transaction records and join primary completion (`loggingreplicator/TransactionManager.cpp:236-516`) |
| `ReplicatedLogManager` | Initiate V1 replication, assign returned LSN, order local append, maintain history (`loggingreplicator/ReplicatedLogManager.cpp:382-396,638-670,731-861,915-957`) |
| `PhysicalLogWriter` | Assign local PSNs/positions and append/flush physical bytes (`loggingreplicator/PhysicalLogWriter.cpp:286-339,600-654,844-866`) |
| `OperationProcessor` | Dispatch redo and unlock by transaction, role, and recovery cutoff (`loggingreplicator/OperationProcessor.cpp:333-356,402-615,665-915`) |
| State Manager | Provider identity, transactional membership, lifecycle and apply routing (`statemanager/StateManager.cpp:707-858,2796-2859,2982-3057`) |
| `CheckpointManager` | Capture checkpoint boundary, wait stability, perform/complete, coordinate truncation (`loggingreplicator/CheckpointManager.cpp:368-486,963-981,1466-1549`) |
| `RecoveryManager` | Select linked checkpoint, reconstruct records/transactions and replay (`loggingreplicator/RecoveryManager.cpp:117-345,374-526`) |
| `CopyStream` / `SecondaryDrainManager` | History-aware mode selection, provider-state/log transfer and installation (`loggingreplicator/CopyStream.cpp:249-445`; `loggingreplicator/SecondaryDrainManager.cpp:185-235,998-1099`) |
| `VersionManager` | Register snapshot visibility and defer provider-version removal (`loggingreplicator/VersionManager.cpp:133-350`) |

There are two different provider boundaries. The **V1 callback adapter**
exposes epoch, last sequence number, copy context/state, and data loss to V1;
it forwards to the logging replicator. The application implements
**`IStateProvider2`**, which receives transaction apply, checkpoint, and
structured copy callbacks. It does not consume raw V1 streams itself
(`ComStateProvider.cpp:57-100,180-272`;
`loggingreplicator/StateProvider.cpp:59-109`;
`common/IStateProvider2.h:14-155`).

The V1 proxy obtains an assigned LSN from `BeginReplicate`; its separate task
completes through `EndReplicate`. This source study follows that boundary but
does not independently re-prove the V1 quorum implementation
(`ComProxyStateReplicator.cpp:179-243`).

## Native API Contracts

### Transactional surface

| Interface | Contract |
|---|---|
| `ITransactionalReplicator` | Combines transaction manager, provider map, and version manager; readability, persistence, partition, epoch, stable/committed queries, notifications, backup and restore (`common/ITransactionalReplicator.h:12-64`) |
| `ITransactionManager` | Create regular/atomic/atomic-redo transactions; validate provider registration; begin/add/commit/abort and buffered-operation unlock (`common/ITransactionManager.h:10-81`) |
| `IStateProviderMap` | Get, transactional Add/Remove/GetOrAdd, enumeration and parent filtering (`common/IStateProviderMap.h:15-77`) |
| `ITransactionVersionManager` | Register/unregister visibility versions (`common/ITransactionVersionManager.h:10-17`) |
| `IVersionManager` | Determine when checkpoint/provider-version intervals are no longer referenced (`common/IVersionManager.h:10-46`) |

The regular transaction represents an atomic group, but arbitrary custom
provider isolation is not supplied by this interface alone. The managed public
transaction contract explicitly assigns operation isolation to the reliable
state implementation; native apply routing does not acquire a universal
cross-provider data lock
(`common/ITransaction.h:10-31`;
`src/prod/src/managed/Microsoft.ServiceFabric.Data.Interfaces/ITransaction.cs:15-49`;
`statemanager/StateManager.cpp:2796-2842`).

### Three transaction forms

| Form | Submission and encoding |
|---|---|
| Regular `Transaction` | First operation buffered locally; second Add submits first begin and new member; Commit uses a single-operation begin fast path or a separate end record (`common/Transaction.cpp:260-307,418-445`) |
| `AtomicOperation` | One async Add with metadata, undo, redo, provider/context; one operation record (`common/AtomicOperation.h:37-46`; `loggingreplicator/TransactionManager.cpp:262-294`) |
| `AtomicRedoOperation` | One async Add without undo; one redo-only operation record; full copy rather than provider undo on false progress (`common/AtomicRedoOperation.h:10-14,40-48`; `loggingreplicator/TransactionManager.cpp:296-325`) |

Atomic objects do not expose a normal Add-then-Commit sequence. A read-only
regular Commit succeeds without writing a transaction record; its wrapper does
not assign a positive write commit sequence
(`common/AtomicOperation.h:37-46`; `common/AtomicRedoOperation.h:40-48`;
`common/Transaction.cpp:418-484`).

### Commit, abort, disposal, and cancellation

Native regular Commit enters Committing and calls the lower manager. A
successful lower call wins before the wrapper examines timeout or
cancellation. Retriable failures include no-write-quorum,
reconfiguration-pending, replication-queue-full, and service-too-busy. Only
after such a failure does the wrapper check its token/deadline and either
retry or return to Active. It does not automatically Abort on that branch
(`common/Transaction.cpp:208-235,418-477`;
`common/TransactionBase.cpp:197-208`;
`common/TransactionStateMachine.cpp:152-176`).

Consequently, the native timeout/token is not a hard interruption of an
outstanding replication call, and cancellation is not a rollback certificate.
The convenience Commit has infinite timeout/no token. The public managed
`ITransaction` has a parameterless Commit instead; its reviewed wrapper
retries without the native timeout/token overload
(`common/ITransaction.h:39-44`; `common/ITransactionManager.h:41-48,75-81`;
`src/prod/src/managed/Microsoft.ServiceFabric.Data.Interfaces/ITransaction.cs:137-157`;
`src/prod/src/managed/Microsoft.ServiceFabric.Data.Impl/Replicator/Transaction.cs:320-375`).

Public native Abort initiates background work rather than awaiting complete
rollback. Disposal also matters after success: it unregisters snapshot
visibility and releases separately registered transaction lock contexts.
Those are different resources from operation-level contexts
(`common/Transaction.cpp:131-179`;
`common/TransactionBase.cpp:161-178`).

Concurrent operations on one transaction are unsupported/misuse; different
transactions may run concurrently. Internal state locks do not turn a
transaction into an arbitrarily thread-safe application object
(`common/ITransaction.h:29-31`;
`common/TransactionStateMachine.cpp:264-286`;
`src/prod/src/managed/Microsoft.ServiceFabric.Data.Interfaces/ITransaction.cs:46-49,109-123`).

## Sequence Numbers and Record Model

| Quantity | Meaning at this revision |
|---|---|
| Transaction ID | Identity independent of log ordering; native atomic IDs are negative (`common/TransactionBase.cpp:78-127`) |
| Operation LSN | V1-assigned logical sequence for a replicated record (`loggingreplicator/ReplicatedLogManager.cpp:915-957`) |
| Transaction commit LSN | Committed end LSN for ordinary multi-operation transactions; own record LSN for single-operation/atomic forms (`loggingreplicator/OperationProcessor.cpp:679-692,726-741,776-847`) |
| Logical tail | Highest appended logical position; insertion can precede physical flush (`loggingreplicator/ReplicatedLogManager.cpp:731-775`) |
| Last stable LSN | Barrier-derived history/stability boundary, separate from commit completion (`loggingreplicator/CheckpointManager.cpp:1186-1204,1554-1621`) |
| Visibility version | Last applied barrier LSN, registered after waiting a captured dispatch barrier (`loggingreplicator/LoggingReplicatorImpl.cpp:577-587`; `loggingreplicator/VersionManager.cpp:276-305`) |
| Physical PSN / position | Local record sequence and byte position, including nonreplicated physical records (`loggingreplicator/PhysicalLogWriter.cpp:286-339,844-866`) |
| Checkpoint LSN | Provider/log recovery cutoff captured for that checkpoint (`loggingreplicator/CheckpointManager.cpp:368-448`; `statemanager/CheckpointManager.cpp:37-39,150`) |

The logging implementation's V1-facing
`GetLastCommittedSequenceNumber` returns the logical log tail. It does not
return “last application transaction commit” or “last stable”
(`loggingreplicator/LoggingReplicatorImpl.cpp:435-444`).

Logical record kinds include begin, operation, end, barrier, epoch update and
backup. Local physical kinds include checkpoint, index, truncation and
information records. Epoch update can reuse the current logical tail LSN;
it is not necessarily another user operation
(`logrecordlib/LogRecordType.h:14-46`;
`loggingreplicator/LoggingReplicatorImpl.cpp:1355-1419`).

## Write, Apply, ACK, and Completion

### Primary: a join, not a quorum-then-apply pipeline

```text
Construct transaction record
          |
Begin V1 replication -> assigned LSN -> own replication task ---------+
          |                                                         |
ordered local append -> flush -> dispatch -> provider apply task ----+
                                                                    |
                                             join apply + replication
                                                       |
                                                 invoke Unlock
                                                       |
                                               return commit result
```

The manager starts V1 replication before local append. Concurrent calls can
receive LSNs before earlier local appends complete, so an ordering queue closes
those gaps. Physical logging appends bytes and flushes a marker before
completing record flush tasks and dispatching them
(`loggingreplicator/ReplicatedLogManager.cpp:382-396,781-845,915-957`;
`loggingreplicator/PhysicalLogWriter.cpp:600-654`;
`loggingreplicator/LoggingReplicatorImpl.cpp:1306-1337`).

For an ordinary transaction, begin/member records build the transaction chain
but do not redo provider data independently. A committed end walks members
forward and applies their redo, passing each operation's LSN and the common
transaction commit sequence. Aborted/pending ordinary transactions do not
redo as committed. Single-operation begin and atomic records apply directly
(`loggingreplicator/OperationProcessor.cpp:665-915`).

Primary completion awaits apply, then its already-running replication task,
invokes engine unlock, and returns. These are concurrent dependencies: source
await order does not prove network completion happened after apply. In fact,
provider apply can happen before the own replication task fails. The inspected
`FailedEndReplicateMustFailCommit` test expects failed commit with one apply and
one unlock
(`loggingreplicator/TransactionManager.cpp:498-526`;
`loggingreplicator/Integration.Test.cpp:1674-1703`).

There is no explicit `lastStableLsn >= commitLsn` predicate in that completion
path. Unlock is invoked before return, but unlock failures are faulted through
a void engine path rather than folded into the transaction's returned
apply/replication status. Thus “commit returned success” must not be
strengthened into “every cleanup succeeded and all background work drained”
(`loggingreplicator/TransactionManager.cpp:498-526`;
`loggingreplicator/OperationProcessor.cpp:463-530,607-615,969-989`).

### Secondary: local durability, optional apply acknowledgement

The secondary deserializes V1 operation bytes, uses the received metadata LSN,
appends locally without re-replicating, and starts a flush/ACK helper
(`loggingreplicator/SecondaryDrainManager.cpp:805-860,1148-1175`).

| ACK mode | Successful replication ACK dependency |
|---|---|
| Ordinary | Successful local record flush |
| `EnableSecondaryCommitApplyAcknowledgement` | Flush plus successful apply for commit-classified records |

Commit-classified means single-operation begin, committed end, or atomic
operation, not an ordinary transaction member or aborted end. After ACK, the
helper still awaits apply; neither mode explicitly waits for unlock/processing
before ACK
(`loggingreplicator/SecondaryDrainManager.cpp:908-971`;
`logrecordlib/LogRecord.cpp:127-147`).

On a secondary, the common processor invokes unlock after apply. A separate
ACK waiter can observe apply completion before that unlock; there is no
universal ACK-versus-unlock total order
(`loggingreplicator/OperationProcessor.cpp:333-356`;
`loggingreplicator/SecondaryDrainManager.cpp:923-961`).

“Speculative” must be qualified: ordinary members are **not** applied before a
commit end. But a locally flushed commit end or self-committing atomic record
can drive provider application before that history is globally stable.
False-progress correction exists for this reason
(`loggingreplicator/OperationProcessor.cpp:719-790`;
`loggingreplicator/TruncateTailManager.cpp:293-321,359-409`).

### Dispatch and stable barriers

The parallel dispatcher batches flushed records until a dispatch boundary,
then processes separate transaction/atomic chains concurrently while
preserving order within a chain. Boundaries include barrier, begin-checkpoint,
epoch update and information records; the commit-apply ACK option also makes
commit records dispatch boundaries
(`loggingreplicator/ParallelLogRecordsDispatcher.cpp:119-150,191-246,303-396`;
`logrecordlib/LogRecord.cpp:396-415`).

A barrier carries the primary's previously known stable LSN. Primary barrier
processing joins its apply/replication and advances stability to the barrier's
own LSN. A secondary advances to the stable LSN **carried inside** the applied
barrier. Idle-primary logic schedules another barrier when needed to propagate
stability
(`loggingreplicator/CheckpointManager.cpp:812-829,1144-1219,1554-1621`).

Stability releases retained completed transaction chains and permits pending
checkpoint/truncation work. It is not the same mechanism as releasing provider
operation contexts at unlock
(`loggingreplicator/TransactionMap.cpp:179-207`;
`loggingreplicator/CheckpointManager.cpp:963-981,1620-1621`;
`loggingreplicator/OperationProcessor.cpp:455-530`).

## `IStateProvider2` Contract and Resource Lifetime

### Identity and callbacks

| Group | Native provider responsibilities |
|---|---|
| Identity | Name and child descriptions; factory receives name, type, persistent ID, partition/replica IDs and initialization data |
| Lifecycle | Initialize with weak transactional handle, directory and child instances; Open, ChangeRole, Close, Abort, RemoveState |
| Application | Apply with operation LSN, transaction, context, metadata/data; return optional operation context; later Unlock |
| Checkpoint | Prepare, Perform, Complete, Recover; checkpoint-only Backup/Restore |
| Copy | GetCurrentState; target Begin/Set numbered records/End |
| Removal | PrepareForRemove in the removing transaction |

Evidence: `common/IStateProvider2.h:14-155`;
`statemanager/FactoryArguments.h:15-40`;
`statemanager/IStateProvider2Factory.h:17-22`.

`ApplyContext` combines role and operation kind. Declared roles are primary,
secondary and recovery. Kinds include redo, unlock, undo and **false progress**
as a distinct value; `SecondaryFalseProgress` is an explicit composite.
An enum member's existence is not proof that every provider supports every
combination
(`common/ApplyContext.h:18-113`;
`statemanager/StateManager.cpp:1716-1819,2796-2842`).

### Context acquisition is not limited to Apply

There are original operation contexts supplied during Add and contexts
returned by Apply. State Manager wraps them with provider identity and routes
Unlock back to that provider. Apply's signature does not receive the original
Add context
(`common/ITransactionManager.h:33-73`;
`common/IStateProvider2.h:143-155`;
`statemanager/StateManager.cpp:984-999,1347-1386,2796-2859`).

Unlock can happen **without Apply**: aborting the still-buffered first
operation directly releases its supplied context. Provider-name lock contexts
are another category, registered on the transaction and released by disposal.
Metadata unlock removes unapplied transient additions or reverses transient
deletion bookkeeping
(`common/Transaction.cpp:260-280,338-354`;
`common/TransactionBase.cpp:161-178`;
`statemanager/StateManager.cpp:4032-4053`;
`statemanager/MetadataManager.cpp:1000-1076`).

Providers own their data isolation and lock/version management. The test
provider acquires its own transaction-scoped lock before adding data and uses
it for removal exclusion. State Manager's name locks are not a substitute
for those data locks
(`statemanager/TestStateProvider.cpp:79-145,946-986`;
`statemanager/MetadataManager.cpp:675-768`).

Callbacks are not globally single-threaded. Prepare walks the prepared
provider set sequentially, while several other lifecycle/checkpoint/copy
callbacks run concurrently **across providers**. The reviewed interface does
not give a complete same-provider reentrancy matrix; phase-exclusion
assumptions and dispatch ordering must be considered separately
(`statemanager/ApiDispatcher.cpp:324-351,498-658`;
`statemanager/StateManager.cpp:167-170,4985-4988`).

## Provider Registry, Hierarchy, and Removal

Names must be valid nonempty URIs with a scheme; the reserved State Manager
name is rejected. IDs are separately generated and persisted, not hashes of
names. Reusing a name therefore does not make an old ID refer to the new
provider
(`statemanager/StateManager.cpp:4622-4667,5264-5291`;
`statemanager/MetadataManager.cpp:928-981`).

Add obtains a name lock, creates provisional metadata and records the
registry mutation in a transaction. Plain Get hides transient creation.
Committed insertion initializes/opens/recovers/role-transitions the provider
tree before exposing it. Remove marks transient deletion and calls provider
PrepareForRemove; committed apply removes active membership while retaining
deleted metadata
(`statemanager/StateManager.cpp:707-785,4305-4451,4797-4920`;
`statemanager/MetadataManager.cpp:271-295,928-981`).

GetOrAdd uses a transaction read lock, then releases/reacquires a write lock
and rechecks a miss. It is not identical to plain Get followed by Add.
Enumeration returns an active provider dictionary, not a snapshot transaction
over every provider's contents
(`common/IStateProviderMap.h:58-77`;
`statemanager/StateManager.cpp:333-459,2872-2937`).

Children come from provider descriptions and factory construction, not URI
prefix inference. Registration stages descendant operations and root in the
same transaction; recovery uses persistent parent/child metadata to initialize
the tree
(`statemanager/StateManager.cpp:737-858,3546-3567,3926-4006,4926-4965`).

Checkpoints/copy retain IDs, descriptors, create/delete LSNs and tombstones.
Native metadata stores operation Apply LSNs; interoperability checks also
accept the managed commit-LSN representation. This is another reason not to
conflate operation and transaction positions
(`statemanager/CheckpointManager.cpp:359-404`;
`statemanager/StateManager.cpp:1892-1933,2134-2145,2326-2337`).

Physical deletion waits for safe-LSN cleanup and lifecycle removal/close.
Removal commit does not immediately delete files; tombstones also support
duplicate replay and false-progress repair
(`statemanager/StateManager.cpp:1957-2025,5294-5418`;
`statemanager/CheckpointManager.cpp:94-148`).

## Checkpoints and Retention

### Three phases and their actual boundary

1. A local begin-checkpoint records history and the earliest pending
   transaction. After preceding dispatch and flush, synchronous provider
   Prepare captures the checkpoint version.
2. Prepare may precede stability. **Perform** waits until the checkpoint
   boundary becomes stable, then performs provider I/O.
3. Under backup/copy consistency coordination, logging writes/flushed
   end-checkpoint, completes providers/state-manager checkpoint, then
   writes/flushed complete-checkpoint.

Evidence: `loggingreplicator/CheckpointManager.cpp:368-448,963-975,1251-1325,1466-1549`;
`loggingreplicator/ReplicatedLogManager.cpp:620-635`.

State Manager retains the prepared provider/metadata set. Providers added after
Prepare are excluded; providers removed after Prepare can still receive
Perform/Complete. Perform updates the copy/checkpoint snapshot, so copy can
select either the prior or newly performed snapshot according to ordering;
it need not wait for Complete
(`statemanager/CheckpointManager.cpp:37-209,537-631`;
`statemanager/StateManager.cpp:4973-4998`;
`statemanager/Checkpoint.Test.cpp:64-118,220-265`).

Complete must tolerate restart: Open can finish state-manager file replacement,
instantiate/open providers, invoke their Complete with no same-instance
Prepare/Perform, and only then RecoverCheckpoint. The file-state completion
code handles temporary/current/backup combinations; it is not merely a
volatile callback chain
(`statemanager/StateManager.cpp:2982-3057`;
`statemanager/CheckpointManager.cpp:220-267`;
`statemanager/CheckpointFile.cpp:40-150`).

This source does not establish that every provider's image is immutable or
that similarly named file replacements have identical crash guarantees on
every platform. Provider storage behavior and underlying filesystem/log
implementation remain separate evidence
(`statemanager/StateManager.cpp:1469-1503`;
`loggingreplicator/FileLogManager.cpp:277-371`;
`loggingreplicator/KLogManager.cpp:678-713`).

### Independent retention constraints

| Retention reason | Mechanism |
|---|---|
| Checkpoint recovery and open transaction | Head must retain the completed checkpoint and any earlier pending transaction (`loggingreplicator/ReplicatedLogManager.cpp:510-555,714-728`) |
| Completed but unstable transaction | Keep chains needed for false-progress correction until stable cleanup (`loggingreplicator/TransactionMap.cpp:128-207`) |
| Copy/backup physical reader | Registered reader ranges postpone physical truncation (`loggingreplicator/LogManager.cpp:625-656,698-827`) |
| Snapshot reader | Visibility registrations defer provider-version/checkpoint removal (`loggingreplicator/VersionManager.cpp:133-350`) |
| Unstable head-truncation/checkpoint work | Stable gate prevents premature processing (`loggingreplicator/CheckpointManager.cpp:454-486,963-981`) |

Version removal tests whether registered visibility falls in the version
interval `[commitLsn, nextCommitLsn)`. Duplicate snapshot registrations are
counted; the last unregister signals release. This is different from physical
reader pins and transaction-chain retention
(`loggingreplicator/VersionManager.cpp:133-254,276-350`).

Pressure policy can identify old pending transactions for abort. It is not
safe to summarize GC as “delete everything below last commit”
(`loggingreplicator/LogTruncationManager.cpp:118-188`;
`loggingreplicator/CheckpointManager.cpp:1092-1095`).

## Open, Recovery, and Role Changes

The wrapper opens V1 before V2 and aborts V1 on V2-open failure. Native V2
opens logging to obtain recovery information, opens State Manager using those
flags, then runs log replay. It marks its own service open early enough for
provider recovery callbacks, not as permission for clients to serve
(`ComTransactionalReplicator.cpp:674-710`;
`TransactionalReplicator.cpp:257-330`).

Recovery follows physical checkpoint links, not the highest numeric
begin-checkpoint. An end record identifies its completed begin-checkpoint by
position offset. An end without matching complete-checkpoint causes provider
completion during Open and a missing completion marker after recovery
(`loggingreplicator/RecoveryManager.cpp:117-345,613-623`;
`logrecordlib/RecoveryPhysicalLogReader.cpp:78-105`).

Replay begins at the selected checkpoint's earliest pending-transaction
position, which may precede the checkpoint. It reconstructs transaction links
and history before applying. Redo is suppressed when the **transaction commit
LSN**, not each member's LSN, is at or below the recovered/copied checkpoint.
This permits a transaction begun before the checkpoint but committed after it
to replay all members correctly
(`loggingreplicator/RecoveryManager.cpp:374-526`;
`loggingreplicator/LogRecordsMap.cpp:276-373`;
`loggingreplicator/OperationProcessor.cpp:776-903`).

Recovery dispatches through normal processing, drains apply/processing, and
fails on apply/unlock errors. A promoted primary aborts inherited unfinished
transactions. Recovery reconstructs local history; subsequent selected-primary
copy can still require false-progress repair. It does not prove every local
committed record globally stable
(`loggingreplicator/RecoveryManager.cpp:441-526,586-648`;
`loggingreplicator/OperationProcessor.cpp:187-209,979-987`;
`loggingreplicator/LoggingReplicatorImpl.cpp:1006-1014,1136-1152`).

Role changes run the inner V1 transition before V2. Primary/idle-secondary
logging work precedes provider role notification; active-secondary coordinates
dispatch pause and provider notification. Idle-to-active waits copy drain, not
the end of the long-running replication stream. None transitions providers and
deletes the log
(`ComTransactionalReplicator.cpp:854-884`;
`TransactionalReplicator.cpp:402-442`;
`loggingreplicator/LoggingReplicatorImpl.cpp:891-916,1434-1467`).

Epoch updates validate history and previous-epoch tail, reuse the current tail
LSN for the epoch record, update the progress vector, and flush/process it.
New-primary work establishes a barrier before normal transactional work
(`loggingreplicator/LoggingReplicatorImpl.cpp:611-639,817-829,1355-1419`;
`loggingreplicator/ReplicatedLogManager.cpp:588-617`).

Read/write admission is layered. State Manager Get/enumeration check
readability; its secondary-readable fallback does not bypass a denied primary
read. Its write-admission helper rejects NotPrimary but can allow transient
denials to begin work, leaving replication to decide progress. This is not a
guarantee that admitted work will commit, nor a rule that Kuberic should copy
that policy
(`statemanager/StateManager.cpp:218-271,4670-4751`).

## History, False Progress, and Replica Copy

### Mode selection

Progress vectors identify shared epoch/history, not just equal numeric tails.
Mode selection considers retained source/target log, trimmed history, data-loss
discontinuity, and target progress beyond the shared point
(`logrecordlib/ProgressVector.cpp:131-257`).

| Mode | Meaning |
|---|---|
| None | Same latest history and tail |
| Partial/log copy | Sufficient common history and retained log |
| False progress plus partial copy | Target must first discard/undo divergent history |
| Full copy | Common history/log unavailable or rollback unsafe, including redo-only divergence |

Evidence: `logrecordlib/ProgressVector.cpp:131-257`;
`logrecordlib/CopyModeResult.cpp:75-93`.

### False progress

Rollback is restricted to secondary history above the stable/checkpoint
floor. It walks backward, applies undo with `SecondaryFalseProgress`, repairs
transaction links and progress vector, then truncates physical/logical tail.
It also resets read-consistency/barrier state; removing bytes alone is not the
whole operation
(`loggingreplicator/TruncateTailManager.cpp:84-99,138-179,213-321,359-448,548-555,760-787`;
`loggingreplicator/SecondaryDrainManager.cpp:1214-1268`).

Atomic redo-only is explicitly **not** undone. Recovery remembers its last
LSN; if the required rollback crosses it, selection returns
`AtomicRedoOperationFalseProgressed` and full copy. Redo-only therefore means
“rebuild instead of undo,” not “false progress cannot happen”
(`common/AtomicRedoOperation.h:10-14`;
`loggingreplicator/LogRecordsMap.cpp:330-335`;
`logrecordlib/CopyModeResult.cpp:75-93`;
`loggingreplicator/TruncateTailManager.cpp:224-250`).

### Full-copy protocol

The source waits for its requested boundary to flush, selects a provider-state
stream and retained log reader, then sends provider state, history/checkpoint
metadata and logical log records. Full-copy log can start **before** the
checkpoint to preserve an open transaction. The requested upper boundary may
be raised to the selected checkpoint's stable boundary
(`loggingreplicator/CopyStream.cpp:165-175,249-349,420-445,537-552`;
`loggingreplicator/CheckpointManager.cpp:621-670`).

State Manager sends registry metadata before provider streams and wraps
records with provider IDs. A null provider stream means empty state. The
destination may reuse an existing provider instance, not necessarily create a
fresh one
(`statemanager/CheckpointManager.cpp:560-631`;
`statemanager/StateManager.cpp:3678-3775`).

The destination calls Begin, numbered Set callbacks, and End on successful
state-transfer completion. **End is explicitly not guaranteed when copy
aborts.** Providers must not rely on End as a finally/cleanup callback
(`common/IStateProvider2.h:109-135`;
`loggingreplicator/SecondaryDrainManager.cpp:998-1099`;
`statemanager/StateManager.cpp:5753-5768`).

The target creates a separate copy log and stages its first checkpoint.
At the copy boundary it flushes, writes end-checkpoint, publishes/renames the
copy log, completes providers, and writes complete-checkpoint. Incomplete
copy cancels that first checkpoint. Copy drain finishes before the receiver
obtains/drains the replication stream
(`loggingreplicator/SecondaryDrainManager.cpp:185-235,415-433,596-605,654-690`;
`loggingreplicator/CheckpointManager.cpp:294-344,1393-1505`).

The file and KLog backends use different replacement primitives. Their
call-site ordering establishes staged publication but is not an independent
proof of every platform's crash-atomic filesystem behavior
(`loggingreplicator/FileLogManager.cpp:277-371`;
`loggingreplicator/KLogManager.cpp:678-713`).

## Backup, Restore, and Data Loss

Provider BackupCheckpoint/RestoreCheckpoint concern provider images.
Provider restore makes a checkpoint current **without recovering it**.
Replica-level backup additionally needs registry/history/log and subsequent
recovery
(`common/IStateProvider2.h:78-100`;
`statemanager/StateManager.cpp:1558-1656`).

Full backup captures provider checkpoints and required log under consistency
coordination. Incremental backup captures a continuation log range without
another provider checkpoint backup. Backup metadata records parent identity
and versions. The runtime invokes the user destination callback, then
replicates a backup completion record and processes a later barrier before
returning. Callback success is not identical to complete API success
(`loggingreplicator/BackupManager.cpp:189-259,285-406,905-983,1477-1582`).

Incremental eligibility requires a known completed base and retained compatible
history. Crossing replica/configuration epochs needs the relevant setting and
same data-loss version; missing required log/history produces a
missing-full-backup result. Historical incremental availability is not a
promise to retain logs forever
(`loggingreplicator/BackupManager.cpp:707-784,1037-1073`).

Restore validates full/self-parent and incremental parent links, increasing
backup LSNs, and log files. It is allowed only in the data-loss-handler window.
Safe policy checks that backup progress is ahead under the epoch/LSN policy;
Force omits that check, not all chain/format validation
(`loggingreplicator/BackupFolderInfo.cpp:431-513`;
`loggingreplicator/BackupManager.cpp:430-479,1840-1904`;
`loggingreplicator/LoggingReplicatorImpl.cpp:1565-1591`).

Restore records an in-progress token before destructive work, restores provider
checkpoints and log, recovers, reapplies the data-loss epoch, transitions to
primary, and removes the token. An outstanding token outside the data-loss
window triggers cleanup on Open instead of silently reusing mixed state
(`loggingreplicator/BackupManager.cpp:113-129,493-555,1614-1631`;
`statemanager/StateManager.cpp:3104-3157`).

## Native and Managed API Differences

Do not combine the two into one supposedly identical signature set.

| Concern | Native | Managed |
|---|---|---|
| Provider identity | Factory receives identity; Initialize gets weak handle/directory/children | Initialize receives name, ID, initialization bytes and directory directly |
| Prepare | Synchronous PrepareCheckpoint | Async PrepareCheckpointAsync; Perform also has a mode |
| Copy Begin/End | Async Begin; End has token | Synchronous Begin; async End without token |
| Extra provider hooks | No provider OnDataLoss/OnRecoveryCompleted in this interface | Both declared in internal provider interface |
| Public Commit | Infinite convenience overload plus timeout/token overload | Parameterless public CommitAsync |
| Default backup | Native defaults not established by declarations alone | Public contract documents Full and one hour |

Evidence: `common/IStateProvider2.h:19-155`;
`statemanager/FactoryArguments.h:15-40`;
`common/ITransaction.h:39-44`;
`common/ITransactionalReplicator.h:46-64`;
`src/prod/src/managed/Microsoft.ServiceFabric.Data.Impl/Replicator/IStateProvider2.cs:56-59,82-85,100-149`;
`src/prod/src/managed/Microsoft.ServiceFabric.Data.Interfaces/ITransaction.cs:137-174`;
`src/prod/src/managed/Microsoft.ServiceFabric.Data.Interfaces/IStateProviderReplica.cs:82-144`.

A cancellation parameter is not proof of prompt cancellation. Native metadata
lock helpers accept a token but their actual lock await uses timeout without
passing that token. Copy dispatch passes None to provider Set/End in the
reviewed path
(`statemanager/StateManagerLockContext.cpp:35-108`;
`statemanager/StateManager.cpp:5738-5742,5759-5765`).

## Existing Test Evidence

These tests were inspected, not run. State Manager tests use mock logging/test
providers in several scenarios, so metadata assertions do not independently
prove distributed durability or arbitrary provider image persistence
(`statemanager/test/CMakeLists.txt:8-69`;
`statemanager/TestStateProvider.cpp:815-836,989-1017`).

| Scenario | Source / assertion |
|---|---|
| Pending transactions do not redo | `loggingreplicator/OperationProcessor.Test.cpp:309-339` |
| Committed member data/order | `loggingreplicator/OperationProcessor.Test.cpp:226-249` |
| Failed flush suppresses apply | `loggingreplicator/OperationProcessor.Test.cpp:412-435` |
| Replication failure after apply fails commit | `loggingreplicator/Integration.Test.cpp:1674-1703` |
| Recovery cutoff / checkpoint-spanning transaction | `loggingreplicator/OperationProcessor.Test.cpp:343-368`; `loggingreplicator/Integration.Test.cpp:851-905` |
| End-checkpoint without complete marker | `loggingreplicator/Integration.Test.cpp:909-957` |
| Physical reader delays truncation | `loggingreplicator/Integration.Test.cpp:1405-1450,1473-1527` |
| Full-copy boundary and restart | `loggingreplicator/Integration.Test.cpp:2775-2786,2887-2900,2923-2940` |
| Redo-only divergence forces full copy | `loggingreplicator/ProgressVector.Test.cpp:1158-1209` |
| Prepared provider set survives add/remove | `statemanager/Checkpoint.Test.cpp:64-118,191-265` |
| Repeated/restarted checkpoint Complete | `statemanager/CheckpointManager.Test.cpp:880-956`; `statemanager/StateManager.Test.cpp:3215-3274` |
| Provider copy instance reuse / metadata replay | `statemanager/Copy.Test.cpp:1736-1883` |
| Provider removal versus data exclusion | `statemanager/StateManager.Test.cpp:3141-3206` |
| Invalid backup lineage | `loggingreplicator/BackupFolderInfo.Test.cpp:321-502` |

## Qualifications and Research Boundaries

This is a source-backed semantic map, not an assertion that every native
implementation branch fulfills an idealized contract:

- Native atomic retry code contains a bare Active enum expression rather than
  a state assignment at this revision. Do not infer guaranteed object
  reusability after retriable atomic failure from the intended state diagram
  (`common/TransactionStateMachine.cpp:84-95`).
- Replication ACK errors suppress ACK explicitly, but the copied-log batch
  helper signals a void batch completion even on per-record failure. Its
  error handling needs separate analysis before claiming every copy ACK
  certifies successful apply/durability
  (`loggingreplicator/SecondaryDrainManager.cpp:559-631,713-765,932-943`).
- Backup's stabilization/status-combination code has paths that do not
  straightforwardly propagate each underlying status. This research does not
  claim a dynamically reproduced backup failure or treat those paths as
  normative behavior to copy
  (`loggingreplicator/BackupManager.cpp:325,1519-1582`).

Not established here: a full same-provider callback concurrency matrix, the
default value and all supported combinations of the secondary-apply ACK
setting, arbitrary-corruption/torn-tail salvage guarantees in the underlying
logger, every native convenience-overload default, or end-to-end formal
reconfiguration safety. Source signatures, comments, unit tests, and observed
ordering are kept distinct rather than filling those gaps with assumptions.

## Implications for Reading the Kuberic Proposal

The [Kuberic design](../../features/kuberic/v2-transactional-replicator-design.md)
now targets a faithful Rust port of native SF V2. It retains the `common`,
`logrecordlib`, `loggingreplicator` and `statemanager` component layout and
the transaction/apply/ACK/stability/recovery semantics described here.
Rust async, ownership, a Rust logical-log backend and Kuberic hosting replace
the native platform machinery, not the protocol.

The earlier committed-only atomic-redo proposal is superseded. Dynamic
providers, regular transactions, undo-capable false progress, version
retention and partial copy are part of the target port, not permanent
omissions. SQLite may consume a smaller subset without reducing the engine
contract. Protected V1 interfaces remain unchanged; private early-LSN,
operation-stream, copy-context and readiness bridges are required.

The source qualifications above still apply: a faithful port needs explicit
decisions and tests for observed native anomalies, rather than treating every
implementation defect as normative. This document remains research evidence,
not a claim that the port is implemented.
