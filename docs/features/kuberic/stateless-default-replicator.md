# Stateless Default Replicator

## Status

Alignment Phase 6 is delivered as a dormant, feature-gated
`StatelessPreview` beside the legacy managed default engine. The preview
implements the Service Fabric V1 ownership and recovery model in this document
through protocol 11/schema 11, but it is selectable only by repository
test/development construction. Production remains on protocol 9/schema 6 and
the legacy engine. Phase 7 is the next step: remove the legacy path, build the
replacement-only artifact and perform the breaking fresh-storage cutover.

## Overview

Kuberic's production default replicator still participates directly in durable
metadata management. The host injects SQLite-backed authority, replication
progress, local-write, build, removal and retirement stores into that legacy
managed engine. This allows exact continuation across many process crash
boundaries, but it also makes the replicator a second durable control-plane
owner alongside the replica agent.

The Service Fabric V1 Replicator uses a simpler boundary:

- the Replicator owns only live replication mechanics;
- the state provider owns durable application state and progress;
- the Reconfiguration Agent (RA) owns topology, role and reconfiguration
  intent;
- a restarted Replicator is reconstructed from those two durable owners.

The Phase 6 preview adopts that boundary. It does not open, write or recover an
engine metadata database; it is a restart-reconstructable data-plane
component. The local replica agent remains durable, but its schema-11 SQLite
state is agent-owned lifecycle/operation authority rather than Replicator
persistence.

The preview remains a built-in implementation, but the runtime hosts it
through the same public interface bundle as an application-provided custom
replicator. No private managed lifecycle, data-plane, observation, fencing or
receipt capability may cross that boundary. This change does not require
changing the public `Replicator` or `PrimaryReplicator` interfaces.

## Goals

1. Make every default-replicator instance disposable and reconstructable after
   process restart.
2. Make the state provider the sole durable authority for application bytes,
   applied progress, committed progress and retained application history.
3. Make the local replica agent the sole durable authority for identity,
   topology, epochs, PC/CC configuration, role intent and reconfiguration
   workflow.
4. Remove default-replicator access to SQLite-backed authority, write-journal
   and build-progress stores.
5. Reconstruct the replication engine by replaying agent-owned state and
   querying state-provider progress.
6. Keep writes closed until reconstructed agent authority and state-provider
   progress have been validated together.
7. Match Service Fabric V1 restart behavior for in-flight writes, replica
   builds and peer sessions.
8. Remove every privileged runtime-to-default-replicator capability and use
   only the public replicator and state-provider interfaces.

## Non-Goals

- Removing durable state from the local replica agent.
- Making persisted applications volatile.
- Replacing the PC/CC, epoch or quorum protocols.
- Changing custom replicator ownership.
- Changing `Replicator` or `PrimaryReplicator`.
- Adopting the Service Fabric V2 Transactional Replicator or a
  replicator-owned write-ahead log.
- Guaranteeing transparent completion of an in-flight client request after the
  hosting process has crashed.
- Preserving or restoring any pre-cutover agent or application data, including
  SQLite, KVStore2 and PostgreSQL.
- Rolling or mixed-version migration.

## Service Fabric Reference Model

The Service Fabric V1 Fabric Replicator has in-memory operation queues,
role-specific engines, peer sessions and acknowledgement state. It does not
recover those objects from a replicator-owned log.

After restart:

1. The RA recreates the service and Replicator.
2. The state provider reports its last committed LSN.
3. The RA drives role, epoch and replica-set configuration again.
4. The Replicator recreates queues and peer sessions from those calls.
5. A replica that cannot catch up from retained application history is rebuilt
   through the state provider's copy stream.

`hasPersistedState` describes the service's durability; it does not enable
Replicator metadata persistence. See
[Service Fabric State Management and Persistence](../../background/service-fabric/state-management.md#v1-restart-and-recovery-ownership).

## Current Kuberic Ownership

The production legacy default engine receives these durable dependencies:

- `ManagedReplicaStore`;
- `ReplicationProgressStore`;
- `LocalWriteJournal`;
- `BuildAuthorityStore`;
- `BuildProgressStore`.

They are backed by the same host SQLite database that stores agent state. The
engine consequently restores admitted authority, authority-fenced replication
progress, incomplete local writes, build authority and build progress into its
own runtime state.

This creates three problems:

1. **Split ownership:** agent and engine both interpret durable topology and
   workflow records.
2. **Recovery coupling:** changing an agent transition often requires matching
   engine persistence and restoration behavior.
3. **Stronger-than-SF continuation:** the engine attempts to resume internal
   work that SF V1 discards and reconstructs.

The SQLite database is still necessary for the agent. The Phase 6 preview uses
a separate fresh schema-11 store only for agent-owned identity, history,
lifecycle intent, operation ownership and terminal public completion. Its
stateless engine does not receive any of the dependencies above.

## Target Ownership

| State | Target owner | Restart behavior |
|---|---|---|
| Replica/PVC identity and incarnation fencing | Agent SQLite | Reload and validate before opening the service |
| Previous/current configuration, epoch and role intent | Agent SQLite | Replay into a new replicator instance |
| Reconfiguration effect intent and completion | Agent SQLite | Reissue or observe through the normal effect protocol |
| Build requirement, exact source/target/session identity and terminal public completion | Agent public build store | Reload and reissue the public build operation |
| Application data, applied tail and committed LSN | State provider / `DurableState` | Query during replicator reconstruction |
| Retained replication operations | `DurableState` | Enumerate when peer catch-up requires them |
| Copy snapshot bytes | `StateProvider` | Regenerate from a frozen committed boundary |
| Replication queues and quorum ACK aggregation | Default replicator memory | Recreate empty |
| Peer sessions, addresses and send windows | Default replicator memory | Recreate from public configuration calls and reconnect |
| Pending client completion | Caller memory | Complete with process failure; caller resolves ambiguity |
| In-progress build stream and sequence cursor | Replicator memory | Cancel and restart from a new authorized attempt |

The agent may persist the fact that a topology operation is required or that a
target replica must be built. It must not persist an engine cursor merely to
continue a particular in-memory copy stream.

## Phase 6 As-Built Preview

### Selection and ownership

The process-wide selection is fixed before store open and listener binding:

- `LegacyManaged` is the ordinary production default. It opens schema 6,
  constructs the managed engine and lets `AgentService` own the replication
  listener.
- `StatelessPreview` exists only with the runtime `testing` feature. Tests
  select it through `ReplicaHost::with_stateless_preview` and an exact
  `PreviewLifecycleBinding`. It creates/opens only schema 11, suppresses the
  agent replication listener and lets `StatelessReplicator::open` own the
  selected endpoint.

`ReplicaProcessConfig`, production manifests and ordinary controller
configuration expose no preview selector. The application-side SQLite and
KVStore2 factory switches are likewise testing-only.

One Open may reserve and register one coherent public bundle. The registration
lease permits one provider writer, and the selected Replicator owns one
listener. Crossed factory/process selection, schema 6, mismatched preview
identity, duplicate creation, listener mismatch or Open failure aborts startup
without legacy fallback.

### Store-free engine

`kuberic-runtime/src/replicator/stateless/` owns only process-local role,
epoch, exact PC/CC authority, peer sessions, acknowledgement aggregation,
build state, listener/descendant tasks and provider-derived progress. It has no
dependency on the managed authority, replication-progress, local-write or
build-continuation stores.

Open reconciles `StateProvider::last_committed_lsn` with
`DurableState::durable_progress`, verifies retained bytes for any
applied-above-committed suffix, and starts access-closed. Role, epoch and
configuration are admitted only by exact Public V1 replay. Replication
acknowledgement follows provider durability; both current and previous
configuration write quorums are required. Any ambiguous provider failure
fences serving until exact suffix settlement or corrective replacement.

### Dynamic recovery and replacement

The preview implements higher-epoch process-session renewal, specific-quorum
and `All` catch-up, failover, switchover, build, removal, data-loss handling
and permanent retirement through the same public methods as an independent
custom Replicator.

Build freezes a provider copy boundary, streams deterministic copy, then
streams every retained operation through a later catch-up boundary before
completion. Incompatible provider or retained history returns
`ReplicaRebuildRequired`. The old storage incarnation is retired permanently;
the replacement starts with empty provider state and must complete a new exact
build before admission. Real SQLite and KVStore2 journeys prove that a retired
higher-progress store is not copied or readmitted after replacement restart.

### Hosted controller chain

The feature-gated `HostedPublicV1Adapter` implements history,
session-renewal, `PublicV1OperationApi` and evidence APIs. An evaluator-accepted
transition becomes one validated `ProtocolCommand::PublicV1Operation`; the
executor dispatches it through the adapter and preview-only runtime command
path to the durable operation owner. Only the matching
`PublicV1AgentReport` completion can be persisted and applied as an exact
accept, retirement or replacement effect. History/session renewal precede
dependent operation/evidence acceptance.

This chain is compiled only by `runtime-test-bridge`; production normalization,
CRDs and dispatch continue to reject Public V1.

### Conformance and security boundary

The shared KSF catalog maps the built-in preview to every KSF-01 through
KSF-15 public-observable result. KSF-09 and KSF-10 also retain negative
replacement-artifact guards while the legacy capability and listener remain.
KSF-16 has no preview scenario and remains the Phase 7 legacy-continuation
guard.

Public V1 replication transport intentionally has no peer authentication or
authorization. Identity, process session, epoch, configuration, history,
storage incarnation, build attempt and operation IDs are correctness fences,
not credentials. The bearer token used by hosted fixtures protects the
separate agent control endpoint, not Replicator peer transport.

## Public V1 Contract

The public boundary includes both construction and runtime interfaces:

- `ReplicatorFactory`, `ReplicatorFactoryContext`, `ReplicatorSettings` and
  `ReplicatorInterfaces`;
- `Replicator`, `PrimaryReplicator` and `StateReplicator`;
- `StateProvider` and `DurableState`.

The protected method sets of `Replicator`, `PrimaryReplicator`,
`StateReplicator`, `StateProvider` and `DurableState` remain unchanged.
Public construction and value types may gain immutable transport configuration
and capability metadata, but they must not carry runtime callbacks,
agent-store handles or host-only lifecycle capabilities.
`ReplicatorInterfaces` returns only the standard public handles
plus immutable `ReplicatorCapabilities` bound to the same creation identity.

`ReplicatorCapabilities` initially contains:

- `catch_up_specific_quorum: bool`;
- `data_loss_replay: Convergent | ReplaceOnAmbiguity`.

Absent specific-quorum support means the runtime must use
`ReplicaSetQuorumMode::All` for both swap waits. `true` allows the runtime to
select write quorum, which still requires the designated `must_catchup`
successor. The data-loss replay value declares whether an ambiguous,
unjournaled callback may be safely reinvoked and reconciled or requires
retirement of the affected incarnation.

Capabilities are fixed for one creation/process session. Every durable
catch-up/data-loss operation records the selected mode/disposition and expected
capability. A catch-up capability mismatch is rejected and re-admitted with a
new explicit mode. A data-loss authorization preserves its original replay
disposition: a replacement creation must support that disposition or take the
stricter retirement path; it can never upgrade `ReplaceOnAmbiguity` to
`Convergent`.

`ReplicaInformation` carries the exact build ID, target identity, process
session and address for the one `build_replica` call. This is build-attempt
identity, not configuration membership and not a private callback. It is
absent from active PC/CC descriptions.

Active remote entries carry exact peer identity, process session, address and
progress. Public primary configuration remains the membership source. A
secondary still does not receive a primary-only configuration callback; its
local role and epoch barriers fence predecessor traffic.

Primary process-session replacement is never authorized within the same epoch.
Any primary session change or peer withdrawal creates a newer
configuration epoch. The runtime first closes affected access/peer ingress,
delivers the public `update_epoch` barrier to every surviving secondary and
only then enters replacement primary role. Fresh primary configuration follows
role and carries the new exact peer descriptors before connection.
`update_epoch` invalidates all predecessor peer sessions. There is no
same-epoch source-session substitution.

Public V1 does not authenticate or authorize transport peers by design. Exact
identity/session checks provide lifecycle correctness but are not proof of the
remote process. TLS identity, workload identity or token authentication may be
added later as a separate transport-security feature; they are not an
activation requirement for Public V1.

The public value contract defines `INVALID_LSN = -1`; zero remains a valid
empty-history boundary. Build descriptors and "Replicator already owns this
peer progress" entries use `INVALID_LSN` rather than constructor-default zero.

This follows SF V1:

- the RA proxy obtains only first/last progress through
  `GetCatchUpCapability` and `GetCurrentProgress`
  (`src/prod/src/Reliability/Failover/ra/ComProxyReplicator.cpp:72-84`,
  `src/prod/src/Reliability/Failover/ra/ReconfigurationAgentProxy.ActionListExecutorAsyncOperation.cpp:801-823`);
- successful public `BuildReplica` completion marks the remote proxy ready;
  the copy-end replication boundary remains internal to the Replicator
  (`src/prod/src/Reliability/Failover/ra/FailoverUnitProxy.ReplicatorBuildIdleReplicaAsyncOperation.cpp:311-350`,
  `src/prod/src/Reliability/Replication/PrimaryReplicator.BuildIdleAsyncOperation.cpp:42-135`);
- the Replicator binds, authenticates and closes its own replication transport
  and returns its published endpoint from Open
  (`src/prod/src/Reliability/Replication/ReplicationTransport.cpp:36-165`);
- the RA sequences public replicator calls and updates service read/write
  status separately rather than using a private access transaction
  (`src/prod/src/Reliability/Failover/ra/ProxyActionsList.cpp:106-147`,
  `src/prod/src/Reliability/Failover/ra/ReconfigurationAgentProxy.ActionListExecutorAsyncOperation.cpp:1136-1144`).

### Public completion evidence

Kuberic should use the same evidence shape:

| Agent operation | Public completion evidence |
|---|---|
| Open, role and epoch | Successful exact `Replicator` call, fenced by durable operation, process session and current revision |
| PC/CC configuration | Successful exact `PrimaryReplicator` configuration call |
| Catch-up or switchover readiness | Successful `wait_for_catch_up_quorum` for the exact current/previous configuration and requested mode |
| Build | Successful `build_replica` for the exact build ID, target identity, process session and address |
| Removal | Durable PC/CC transition using public progress/catch-up evidence, followed by successful `remove_replica` for the exact target |
| Data-loss local processing | Successful `on_data_loss`, followed by fresh local public progress; this does not admit history |
| Data-loss history admission | Successful exact configuration/catch-up validation under the authorized proposed `HistoryContext` |
| Retirement | Agent-owned identity fence plus successful public close or synchronous abort |
| Access publication | Agent-owned ordering after required public calls complete; no replicator receipt |

The agent persists these exact call completions as its own receipts. A receipt
does not claim an internal copy boundary, ACK set, queue position or quorum
value that the public call does not expose.

### Public replay and ambiguity contract

Intent-before-effect persistence means a crash can occur after an external
callback mutates state but before its result is journaled. The new protocol
does not promise universal exactly-once external mutation.

| Public operation | Replay/ambiguity disposition |
|---|---|
| Open, role, epoch, configuration and settings | Exact duplicate input must be convergent. A new process reconstructs a new object and replays current state. |
| Progress queries | Read-only and repeatable; fresh values replace no durable completion evidence. |
| Catch-up wait | Re-evaluate the exact captured configuration, mode and capability. Completion is retained only for the exact operation/revision. |
| Build | A terminal retained success is not reinvoked. An ambiguous live attempt is cancelled by retiring its source/target process sessions and reissued with a new build ID and target generation. |
| Remove | Repeated removal of the exact retired identity must converge to resources absent. It never races a live build. |
| Application role/address | Replayed role must converge; publication uses only the latest exact completion and clears stale addresses. |
| Close/abort/fault containment | Repeated containment is idempotent. Child close failure is normalized after abort cleanup while diagnostics retain the child error. |
| Data loss | A retained result is not reinvoked. If the result is ambiguous, `Convergent` may create a new session-scoped attempt under the same immutable logical authorization after fencing the predecessor; `ReplaceOnAmbiguity` retires the authorized incarnation without reinvoking and rebuilds/reselects history. |

Tests inject a crash after callback success but before applied/completed
persistence for every row. Success means operation-specific convergence or the
declared fail-closed replacement path, not proof that the external callback ran
exactly once.

### Public reporting model

The runtime reports only:

- identity, desired and actual role, access, configuration and retained
  operation state owned by the agent/runtime;
- `current_progress` from `Replicator::current_progress`;
- `catch_up_capability` from `Replicator::catch_up_capability`;
- exact durable completion of public build, catch-up, removal and retirement
  operations.

Controller safety decisions must stop depending on
`verified_replication_lsn`, `current_configuration_quorum_progress`, an
engine-observed `committed_lsn`, or a controller-visible copy/catch-up boundary.
Those are Replicator/provider internals. The new protocol removes them
atomically; mixed old/new report formats are rejected.

For failover, scale-down and reconfiguration decisions, the controller uses
the SF-shaped first/last progress pair, current/previous configuration and
durable completion of the relevant public catch-up/configuration operation.
It does not reconstruct an internal ACK set from reports.

### Public progress semantics

The first/last pair has role-specific SF V1 semantics:

| Lifecycle state | `current_progress` | `catch_up_capability` |
|---|---|---|
| Primary | Highest operation quorum-committed by the current replication queue and durably accepted locally | Earliest exact retained operation available for peer catch-up |
| Idle or active secondary | Highest contiguous in-order operation durably accepted locally and safe to acknowledge | Earliest exact retained/completed operation available from local history |
| Opened before role replay, recovery or unresolved suffix | Provider durable applied tail, usable only for election and history selection | Earliest exact provider-retained operation |
| Closed or aborted | Returns the corresponding terminal error |

Provider commitment remains internal and satisfies
`committed_lsn <= applied_lsn`. A secondary's public current progress may be
above its provider-committed boundary. Promotion must drain and settle that
received/applied suffix through public role, catch-up and data-loss processing
before primary access opens. A primary's public current progress is not an
assigned queue tail or a merely local apply.

Reports distinguish **election-safe** from **serving-safe** state. An
agent-owned recovery status marks the replica as access-closed and recovery
pending while still publishing provider-derived first/last progress. The
controller may use that progress to rank candidates and choose the provisional
primary, but it cannot treat the replica as caught up, healthy for promotion,
or serving-ready until public recovery and catch-up operations complete.

This deliberately differs from SF's persisted-secondary optimization, which
may report the highest in-order received operation before service
acknowledgement. Kuberic reports only the highest contiguous operation already
durably accepted by `DurableState`; process-local receive progress is never
election evidence.

The controller compares public progress only between reports with the same
agent-owned `HistoryContext`:

- resource UID and public protocol generation;
- opaque history ID created at bootstrap and replaced only by an explicitly
  authorized data-loss transition;
- data-loss epoch number.

Ordinary configuration-number and role changes do not create a new history.
Reports with different history IDs, data-loss numbers or protocol generations
are incomparable: the controller must not rank their LSNs, combine them into
quorum evidence or use one as a progress floor for the other.

Per-replica Pod/PVC/storage/process identity remains a separate
`ReplicaIncarnationContext`. A fresh replacement starts election-ineligible.
Successful exact public build completion binds its new storage incarnation to
the source's admitted `HistoryContext`; it does not copy the retired
incarnation's progress floor or identity. Only then may reports from the new
incarnation be compared with other members of that history. Public operation
completion—not numeric equality alone—proves that admission.

## Recovery Model

### Startup

A default-replicator restart follows this order:

1. Open and validate the agent store and storage identity.
2. Load the agent-owned admitted authority and any pending topology effect.
3. Open the application and obtain its `StateProvider` and `DurableState`.
4. Create a new default replicator with no restored engine state.
5. Read the durable applied and committed application boundaries.
6. Start unassigned/access-closed and publish a
   `ProcessSessionRenewalRequired` observation containing provider-derived
   election progress. Do not replay predecessor serving role, PC/CC
   configuration or peer sessions.
7. The controller admits a newer configuration epoch bound to the new process
   session, but does not yet apply primary configuration. A restarted former
   primary is not permitted to resume primary under its predecessor epoch.
8. Close affected ingress and complete the newer `update_epoch` barrier on
   every already-running surviving secondary.
9. Replay the replacement process role under the newer epoch. For primary
   promotion, complete Replicator primary role, the explicit primary epoch
   update and application primary role in their normal order.
10. If no data-loss operation is pending, install the newly authorized
    agent-owned PC/CC configuration.
11. Recreate peer sessions only from that new public configuration and its
    current addresses and process sessions.
12. Reconcile application progress with peer progress.
13. Grant read or write access only after the reconstructed configuration,
    epoch, quorum and application progress satisfy the normal activation
    invariants.

The engine session and generation remain process-local fences. A restarted
engine always has a new session and cannot complete work prepared by its
predecessor. Durable old authority remains recovery input for controller
planning, not an executable peer session for the new process.

### Progress Reconstruction

The state provider supplies two local durable boundaries:

- **applied tail:** the highest durably accepted operation for which exact
  retained bytes remain available;
- **committed boundary:** the highest contiguous operation made authoritative
  by the provider's commit protocol.

The reconstructed engine initializes its next LSN above the applied tail, not
merely above the committed boundary. It initializes externally visible
committed progress from the committed boundary. This matches SF V1's
separation between operations retained by the service and the last committed
sequence number reported during recovery.

Peer progress may prove that additional catch-up is required, but it cannot
silently reinterpret or reduce the provider's durable progress. If the local
state is ahead of the agent-authorized configuration, access remains closed
until public catch-up or data-loss handling settles the suffix.

An applied suffix above the committed boundary is unresolved, not committed
and not disposable in place. Normal restart and explicitly authorized
data-loss recovery have different ordering.

After controller session renewal, normal restart installs the newer authorized
PC/CC configuration, reconstructs peers and may re-replicate an identical
suffix through the normal quorum path. Predecessor configuration remains
planning input only and is never executable in the new session.

An authorized possible-data-loss transition uses this sequence:

1. the controller creates a durable logical `DataLossAuthorization` that binds
   the exact transition/operation, old and proposed `HistoryContext`,
   incremented data-loss number, provisional primary replica/storage
   incarnation, permitted replacement scope and the originally selected
   `data_loss_replay` disposition;
2. the agent validates that only this typed transition may change the
   data-loss number, regress/reset progress or invalidate old election,
   configuration, build and replay evidence;
3. each execution creates a session-scoped `DataLossAttempt` bound to the
   logical authorization, current process session and creation capabilities.
   Starting a successor attempt durably fences the predecessor attempt;
4. the runtime keeps access closed, changes Replicator role, applies the new
   epoch, changes application role and invokes
   `PrimaryReplicator::on_data_loss` before installing peer configuration;
5. the Replicator lets the `StateProvider` change local state, rereads provider
   progress/retained bytes and resets local volatile queues;
6. successful local processing records a pending history admission, not
   serving readiness;
7. the runtime installs the authorized PC/CC configuration under the proposed
   history and reconstructs peer sessions;
8. the Replicator validates exact retained operations and peer history through
   public configuration/catch-up behavior, including when the provider
   returned `Ok(false)`;
9. an identical suffix may be re-replicated and committed through the normal
   quorum path;
10. a compatible result admits the new `HistoryContext` and keeps the normal
   SF V1 volatile-queue reset behavior
   (`src/prod/src/Reliability/Replication/Replicator.OnDatalossAsyncOperation.cpp:30-129`);
11. an incompatible result returns an explicit public
    `ReplicaRebuildRequired` error and remains access-closed;
12. the runtime grants access only after the required public catch-up/data-loss
    calls and fresh public progress complete.

If a process exits after possible provider mutation but before the attempt
result is durable:

- `Convergent` permits a new-session attempt under the same logical
  authorization after the old attempt is fenced. The original replay
  disposition remains immutable even if the new Replicator creation advertises
  different capabilities;
- `ReplaceOnAmbiguity` does not renew the attempt. The authorized replica
  incarnation is retired and recovery selects or builds another incarnation.

Re-admission cannot change `ReplaceOnAmbiguity` into `Convergent` or otherwise
bypass required retirement.

Different bytes at the same LSN are divergent history and can never be merged.
The generic V1 contract does not add an in-place rollback method. If
`StateProvider::on_data_loss` cannot safely replace false progress, that
replica enters the rebuild-required path. Callback success or `Ok(false)` is
never interpreted as history compatibility by itself.

Corrective rebuild uses the existing public build/copy interfaces:

1. the Replicator returns `ReplicaRebuildRequired` through the public operation
   that discovered incompatible history;
2. the runtime records the failed exact operation and keeps role/access closed;
   a target that detects the conflict also reports a permanent fault through
   its public `ReplicatorFactoryContext`;
3. the permanently faulted host, process session, replica incarnation and
   application storage/PVC are never readmitted;
4. the controller provisions a replacement replica/PVC incarnation and
   authorizes a new build ID, source and target process session;
5. the replacement opens with empty provider storage in repair-only
   `IdleSecondary` state;
6. the source receives the exact target identity, process session, address and
   build ID in the `ReplicaInformation` build descriptor;
7. the target checks its local identity, process session, idle role, epoch and
   empty-provider state. The target does not require a primary configuration
   callback and remains outside PC/CC. These checks do not authenticate the
   network peer;
8. build withdrawal aborts and retires the exact target process session. A
   replacement attempt always uses a new target process/storage generation and
   build ID. This uses existing public close/abort fencing instead of a private
   target callback;
9. `DurableState::finish_copy` promotes the copy in the replacement storage;
10. retained catch-up and public build completion settle the new history before
   the replacement can join PC/CC.

Permanent fault is terminal for the old replica incarnation. There is no
in-place clearing operation and no attempt to make that host healthy again.
The replacement receives a new process session and storage identity, so its
ordinary healthy report is not constrained by the old host's latched fault.
If no compatible source exists, the partition remains unavailable pending
explicit data-loss authorization. A corrected branch is admitted under a newer
epoch/history; an LSN is never assigned to different bytes within the same
admitted history.

`ReplicaRebuildRequired` is an error classification carried by existing public
method results, not a new lifecycle method. For a remote copy conflict, the
source `build_replica` result identifies the exact target/build failure and the
target reports its permanent fault through the public partition context. The
agent/controller can therefore replace the target storage without receiving a
private Replicator callback.

A single application LSN is not enough to authorize writes by itself. Write
access still requires:

- exact agent-owned authority;
- a non-regressing epoch;
- valid PC/CC quorum configuration;
- application progress consistent with the admitted authority;
- current peer process sessions;
- completion of any required catch-up or build.

The difference is that these facts are combined during reconstruction rather
than loaded from an engine-owned metadata record.

### Epoch Recovery

The agent replays the authoritative epoch through `Replicator::update_epoch`.
The default replicator forwards it to `StateProvider::update_epoch` together
with the previous-epoch LSN, as it does today.

Successful completion is the state-provider durability boundary for
application-specific epoch handling. The agent records completion of its
effect; the replicator does not separately persist the epoch.

### Pending Writes

An in-flight replication request belongs to the process that accepted it. A
process crash ends that request without reconstructing its completion future.
The client may not know whether the operation became durable or quorum
committed and must resolve the ambiguity with an application operation ID,
read-back, or retry.

The default engine must therefore stop persisting `DurableLocalWrite` records.
After restart:

- committed application operations are represented by the provider's committed
  boundary and retained history;
- durably applied operations above that boundary remain an unresolved retained
  suffix until recovery either recommits the identical bytes or corrects false
  progress;
- uncommitted in-memory reservations are discarded;
- the next LSN is greater than the durable applied tail for the admitted
  history;
- exact application retries remain the application's idempotency
  responsibility.

Kuberic must not silently report an interrupted write as successful. Removing
the journal removes client-future continuation, not operation-settlement
recovery. It does not change the requirement for explicit errors,
durable-before-acknowledgement application semantics, or reconciliation of an
applied-but-uncommitted suffix before serving.

### Replica Builds

Build necessity and target identity remain agent-owned. Copy enumeration,
sequence counters, delivery sessions and partial engine progress become
volatile.

If the source or target process restarts:

1. the old build attempt is fenced by its process session and build identity;
2. any partial stream is abandoned;
3. the agent evaluates whether the target still requires a build;
4. a new build identity and frozen replication boundary are authorized;
5. the state provider regenerates deterministic copy bytes for that boundary;
6. retained operations catch the target up after copy.

The frozen copy/catch-up boundary remains internal to the source Replicator.
`PrimaryReplicator::build_replica` completes only after the target has
acknowledged both copy completion and replication through the boundary captured
when enumeration ended. The agent records success for the exact build ID and
target session; the controller no longer requires matching source/target
reports of that internal boundary.

Scale-up admission therefore uses:

1. durable agent authorization of the exact source, target, build ID and
   target process session;
2. successful public `build_replica` completion on the source;
3. target liveness and identity/configuration reporting;
4. the ordinary public PC/CC configuration transition that promotes the
   target.

The source call is the copy-plus-replication proof, as in SF. Target public
progress remains useful for diagnostics and failover ranking, but it is not a
second independent build-completion receipt.

The target state provider remains responsible for making an exact copy retry
safe. A partially installed copy must either be replaceable by the new attempt
or rejected explicitly; the default replicator does not recover a copy cursor
from SQLite.

### Reconfiguration and Removal

PC/CC authority, switchover intent, secondary-removal preparation, accepted
cleanup and retirement remain durable because they are RA facts, not
replicator facts.

The agent replays the resulting configuration into a fresh engine. The engine
proves catch-up, build and removal only by completing the corresponding public
method. Switchover uses public catch-up configuration plus full catch-up
completion before role changes. Retirement uses agent identity fencing plus
public close/abort. No engine-private topology, removal or retirement receipt
survives restart.

This preserves permanent incarnation fencing while aligning the Replicator
itself with SF V1.

## Access Sequencing

Access is runtime-owned and deliberately separate from Replicator internals.
The runtime follows the SF proxy ordering model:

- promotion replays role and epoch, installs PC/CC configuration, completes
  required catch-up and only then grants application access;
- demotion or supersession first denies externally visible application access,
  then performs the required public configuration/catch-up/role calls;
- close and retirement deny access before public close or synchronous abort.

The built-in Replicator checks the public partition write status from
`ReplicatorFactoryContext` when admitting each new write and independently
checks its own role, epoch and PC/CC configuration. Revocation prevents new
admission; an operation already admitted may settle under the authority that
accepted it. Its caller may observe success, failure or ambiguity, but a stale
completion cannot reopen access or complete a newer agent operation.

This is also part of the public `StateReplicator` contract: an implementation
created for a partition must reject new replication while the public partition
write status is not `Granted`. Custom and built-in implementations receive the
same public context and are held to the same behavior.

The runtime persists intent before the sequence, revalidates its operation and
durable revision before granting access, and remains closed on any failed or
ambiguous public call. There is no cross-component prepare/publish transaction
and no private native access proof.

## Transport Ownership

The default Replicator owns its replication listener, peer-session replacement,
send windows, ACK processing and copy delivery.

- immutable process launch configuration selects `LegacyManaged` or
  `StatelessPreview` before `AgentService` binds any replication listener;
- the same selection is available through public factory construction, and a
  mismatch fails startup;
- `ReplicatorSettings` supplies public bind/publish and build-capability
  verification configuration.
- `ReplicatorFactoryContext` supplies public replica and partition identity.
- `ReplicaInformation` supplies peer address, replica identity, build identity
  and process session. Active PC/CC descriptions carry exact remote peer
  descriptors; only the source-side `build_replica` descriptor carries the
  build attempt identity.
- `Replicator::open` binds the listener and returns the published address.
- `Replicator::close` and `abort` stop the listener and all descendant work.

The agent retains its control and peer-discovery endpoints, but no longer
serves or dispatches replication/copy RPCs for the default Replicator. Public
configuration is the only source of peer membership; exact process-session
checks reject predecessor connections and ACKs. A new primary or secondary
process session requires a newer configuration epoch, completed secondary
`update_epoch` barriers and a fresh primary configuration completion before
traffic. Build attempt identity is not peer membership and the idle target
remains outside PC/CC.

## Write and Acknowledgement Contract

The stateless design depends on a strict application durability boundary:

1. A secondary receives an operation.
2. `DurableState::apply` durably applies it.
3. Only then may the secondary acknowledge it.
4. The primary completes the request only after the configured PC/CC write
   quorums acknowledge it.

The primary's own state provider must expose progress consistent with any
operation considered locally durable. Restart recovery trusts the provider's
durable progress and retained bytes, not a lost in-memory queue.

`DurableState::apply` establishes durable acceptance and may advance
`applied_lsn`. `DurableState::commit` establishes provider commitment and may
advance `committed_lsn`. The following relationship must always hold:

```text
committed_lsn <= applied_lsn
```

The range `(committed_lsn, applied_lsn]` must remain exactly enumerable until
it is committed, removed through an authorized false-progress correction, or
replaced by a build. Merely observing that range on one replica does not make
it committed.

Implementations must reject:

- progress regression;
- different bytes for the same durable operation identity;
- an epoch older than the provider's accepted epoch;
- retained history that conflicts with already applied state;
- copy retries whose bytes differ at the same authorized boundary.

## API and Internal Boundary

The runtime/replicator boundary consists only of these public interfaces:

- `ReplicatorFactory`;
- `ReplicatorFactoryContext`;
- `ReplicatorSettings`;
- `ReplicatorInterfaces`;
- `Replicator`;
- `PrimaryReplicator`;
- `StateReplicator`;
- `StateProvider`;
- `DurableState`.

The built-in factory returns the same public bundle shape as a custom
replicator. The replacement has a separate construction boundary:

- leave the legacy `DefaultReplicatorInner::new` and its stores untouched until
  cutover;
- add a separate store-free replacement constructor and factory;
- after controller session renewal, replay role, newer epoch, PC/CC
  configuration, catch-up, build and removal only through the public
  `Replicator` and `PrimaryReplicator` methods;
- initialize progress from `StateProvider`/`DurableState` during open;
- remove engine calls that write agent SQLite tables;
- move peer transport, acknowledgements, copy delivery and process-local
  fencing behind the default replicator implementation;
- remove `ManagedReplicatorLifecycle`, `ManagedReplicatorDataPlane`, native
  observation and private receipt attachment from hosting;
- keep process-local engine session and generation fencing.

If existing `StateProvider` and `DurableState` methods cannot express a
required correction, `StateProvider::on_data_loss` must either perform it or
leave history incompatible so the Replicator returns
`ReplicaRebuildRequired`. Corrective copy is installed into a new empty replica
storage/PVC incarnation through the existing `DurableState::apply_copy_chunk`
and `finish_copy` methods. Do not add another durability method, create a
host-only backchannel, or add topology, PC/CC or RA workflow state to the
application provider.

## Safety Invariants

The implementation is acceptable only if all of these remain true:

1. A fresh engine cannot grant writes before exact authority replay.
2. Restart never permits epoch, configuration or committed-progress
   regression.
3. A predecessor engine session cannot complete work in its successor.
4. Application acknowledgement occurs only after durable application.
5. PC and CC quorum rules remain unchanged.
6. A stale peer process session cannot contribute an acknowledgement or build
   result.
7. Restarted builds cannot reuse an unauthorized boundary or target.
8. Retirement permanently prevents the retired incarnation from reopening.
9. Ambiguous client writes are surfaced as ambiguous failures, not reconstructed
   successes.
10. Reporting composes agent authority with public progress queries and
    runtime-owned completion records, and does not treat missing live state as
    proof of completion.
11. Restart allocates above the durable applied tail and never overwrites an
    unresolved operation in the same admitted history.
12. Provider-visible committed progress advances only after recovery selects
    and durably commits an authoritative contiguous prefix.

## Migration Plan

### Offline activation boundary

The migration is intentionally breaking and is not a rolling upgrade.
Intermediate implementation phases remain inactive until one atomic
protocol cutover.

For every deployment, including SQLite, KVStore2 and PostgreSQL:

1. disable application and replica access;
2. acquire the exclusive cutover Lease, scale/delete the legacy controller
   deployment, wait for every old controller Pod to terminate, revoke its
   command/signing credential and remove its Kubernetes mutation RBAC;
3. stop all old runtime/replicator processes;
4. delete or replace all old agent and application storage/PVCs;
5. create a fresh installation namespace and controller-owned resource UID for
   the new protocol. The old namespace is never reused; delayed accepted
   namespaced mutations remain quarantined there until it is deleted;
6. deploy the replacement-only controller, runtime, default Replicator and
   application provider artifact that implements the complete public contract
   and contains no legacy selection. The replacement controller acquires the
   Lease with a new protocol generation, namespace-scoped mutation RBAC and
   signing/command credential;
7. initialize fresh agent and provider storage;
8. bootstrap new authority from the empty deployment;
9. build the remaining replicas through the public build path;
10. enable application traffic only after all required public recovery
   operations complete.

No old process may run after the new protocol version is selected. Phase 7
does not preserve, restore, enroll or reinterpret any pre-cutover agent or
application data. Operators must export any data they need through an
application-level mechanism before cutover and reimport it as new application
writes afterward. Backup restoration into the new architecture,
identity-preserving enrollment and in-place provider conversion are deferred
features.

PostgreSQL remains an application-owned custom replicator, but it must
implement the new public progress/build/error contract and starts with fresh
agent and application storage at the cutover. No persisted PostgreSQL identity
or effect compatibility is required.

### Deferred migration features

The following are explicitly outside the alignment roadmap's Phase 7 cutover:

- preserving any agent store across the cutover;
- binding established SQLite or KVStore2 storage to a new agent identity;
- restoring a pre-cutover backup directly into the stateless protocol;
- preserving or rebinding PostgreSQL durable metadata;
- rolling or mixed-version activation;
- physical deletion of obsolete default-engine tables from an existing agent
  database.

These require a separately reviewed enrollment/restore protocol with
authoritative source selection, epoch floors, storage identity and old-
incarnation fencing.

### Phase 1: Adopt Shared Public Operation Semantics

This phase is the default-replicator prerequisite supplied by alignment
Phase 4. It creates one next-protocol lifecycle orchestrator used by strict
public/custom test implementations and the future built-in preview.

- run every public application, Replicator and provider callback under one
  exact operation/task owner with cancellation, draining, revision
  revalidation and durable terminal completion;
- implement distinct initial-primary, failover-promotion,
  same-role-secondary-epoch, planned-swap, build-retirement, close, abort and
  fault recipes;
- require Replicator primary role before configuration callbacks;
- invoke promotion `update_epoch` before application primary role and invoke
  `update_epoch` for a secondary that keeps its role across a newer epoch;
- route `PrimaryReplicator::on_data_loss` after primary roles and before
  configuration, catch-up or access; record value-independent
  false/true/error/ambiguous outcomes while access remains closed;
- cancel and settle the exact build future before `remove_replica`;
- retain/publish application role addresses and make transient fault trigger
  access revocation plus restart/drop;
- update the real controller evaluator/executor so transient fault produces an
  exact persisted-restart or volatile-drop/replacement action instead of a
  healthy diagnostic report;
- normalize child graceful-close failure after abort containment while retaining
  diagnostics;
- implement the operation-specific public replay/ambiguity table rather than a
  universal exactly-once promise;
- validate the recipes with a strict trace Replicator/provider whose callbacks
  can reject wrong order or block until cancelled;
- keep the entire next-protocol path preview/test-only until the atomic
  cutover.

The dormant alignment implementation now supplies the complete Phase 4
ordering, replay and fault-control prerequisite used by this roadmap. Planned
swap freezes both exact
configurations plus one opaque mode, reinstalls the relevant configuration
before each recovered wait, revokes serving between waits, and preserves
Replicator-before-application handoff. Exact build attempts are drained before
removal; ambiguous attempts require containment and retirement before a new
attempt. Close/Abort preserve ordered containment and typed child diagnostics.
The agent journal implements the operation-specific replay table above, while
fresh process sessions remain unassigned and cannot execute predecessor
program records. These are preview semantics only and do not select the
replacement default Replicator.

Explicit persisted/volatile classification is frozen with preview/resource
identity. Fault reporting closes local access/location before controller
availability; the repository preview controller persists one exact action and
removes routing before dispatch. Persisted restart uses a durable
accepted/contained/successor handshake and a real same-storage child process;
volatile and permanent faults freeze/delete the old resources and create
distinct scaffolding. The fresh persisted child is intentionally
unassigned/access-closed. Phase 2 still owns higher-epoch session renewal,
history admission and serving authority.

Exit criteria:

- every public callback has one exact task owner, bounded cancellation and
  terminal durable disposition;
- ordering-only traces for initial primary, failover, same-role epoch, swap,
  build retirement, close, abort and fault pass without depending on Phase 2
  progress/configuration values;
- true or ambiguous data-loss outcomes remain access-closed and pending rather
  than fabricating history compatibility;
- controller-driven fault tests prove immediate access closure and exact
  persisted-restart/volatile-drop behavior for the faulted incarnation;
- no Phase 1 lifecycle path is selected by the legacy production protocol.

### Phase 2: Migrate Public Values, Evidence and the Conformance Oracle

This phase is the implementation breakdown for alignment Phase 5. It changes
the runtime/controller contract under a new protocol version that is not
activated against the legacy production engine.

This phase is implemented as the dormant protocol-11/schema-11 Public V1
contract. Its shared test-only catalog executes exactly KSF-01 through KSF-16
for the strict custom fixture and applicable PostgreSQL scenarios. KSF-09,
KSF-10 and KSF-16 remain executable negative activation guards; the Phase 6
replacement engine and Phase 7 replacement-only artifact are not implemented
by this phase.

- add the public configuration value required to identify `must_catchup`
  without changing protected method sets;
- add immutable `ReplicatorCapabilities` to the coherent public bundle with
  catch-up-specific-quorum and data-loss replay declarations;
- define `HistoryContext` and typed `DataLossAuthorization`, update
  `TransitionKind::DataLossRecovery`, controller command/effect stages, durable
  agent state and validation; permit data-loss-number change or progress reset
  only under that authorization;
- add exact build ID, target identity, process session and address to the
  source-side build descriptor; validate local role/epoch/identity/session and
  storage state at the target;
- add exact peer identity, process session and address to active remote
  configuration entries, without a primary-only callback on the secondary;
- prohibit same-epoch primary process-session substitution. Every replacement
  primary session requires a newer configuration epoch, access/peer-ingress
  closure, completed secondary `update_epoch` barriers and fresh peer
  configuration before traffic;
- retire the exact target process/storage generation to revoke or replace a
  build; do not install primary configuration on the idle target;
- project only up, ready remote secondaries into current/previous
  configurations and exclude the local primary plus all idle/in-build targets;
- use invalid/unknown progress in build descriptors instead of source progress
  or synthetic zero; define the public sentinel as `INVALID_LSN = -1`;
- define planned-swap catch-up-specific-quorum capability, successor inclusion
  and the `All` fallback;
- map every controller decision and report field to public first/last progress
  or exact durable public-call completion;
- remove controller decisions that require engine-private verified, quorum,
  committed or build-boundary evidence;
- replace native topology/build receipt fields in durable outcomes with exact
  public-call completion and agent-owned fences;
- define role-specific election-safe and serving-safe progress;
- reject numeric comparison across different resource, protocol, history or
  data-loss contexts; keep storage generation as a replica fence and require
  exact build completion before a new incarnation becomes election-eligible in
  the admitted history;
- define `StateReplicator` completion, secondary stream validity and
  `StateProvider` epoch, previous-tail, committed-progress, copy and data-loss
  semantics;
- define `ReplicaRebuildRequired` propagation through public errors, durable
  operation failure and public partition fault reporting;
- add a new agent-owned public build store containing exact attempt identity
  and terminal public completion but no engine continuation cursor;
- update PostgreSQL and other custom replicators to satisfy the new public
  role/epoch, progress, build identity, error and data-loss replay
  semantics. PostgreSQL must accept primary role before configuration, accept
  epoch barriers without requiring preinstalled configuration, replace
  configuration-membership build validation with exact target identity,
  and replace its current unsupported data-loss callback with either exact
  timeline/history validation or a typed rebuild-required result;
- bump the report/protocol/store versions and reject every old persisted
  format;
- turn the Phase 1 trace implementation into one table-driven conformance
  oracle covering every KSF-01 through KSF-16 disposition.

Controller work:

- update `kuberic-runtime/src/protocol/types.rs`, `protocol/command.rs` and
  `protocol/validation.rs` for protocol/history generations,
  `DataLossRecovery` and session renewal;
- update `kuberic-controller/src/evaluator.rs` and transition modules to compare
  only compatible histories, select provisional rather than serving-ready
  primaries, advance epoch for process replacement, decide data-loss recovery
  versus rebuild and retain exact peer/build identity;
- update `kuberic-controller/src/executor.rs`, `reconciler.rs` and
  `cluster_api.rs` to deliver fenced typed commands and reject stale
  protocol/resource/session identities;
- update `kuberic-controller/src/crd.rs`, normalization and runtime reporting so
  status contains public progress, history/incarnation identity and exact
  public-operation completion but no private verified/quorum/committed/build
  boundary;
- extend controller protocol/model/integration tests to cover delayed reports,
  incomparable histories, replacement session renewal, data-loss
  authorization, exact build/peer identity and corrective replacement.

Exit criteria:

- capability, history, data-loss and build-attempt values have one public
  representation and deterministic durable replay rule;
- ordinary failover still rejects a data-loss-number change, while a typed
  controller authorization invalidates old history/election/build/replay
  evidence and remains access-closed until admission completes;
- crash renewal fences the predecessor `DataLossAttempt`; changed creation
  capabilities cannot weaken the logical authorization's captured replay
  disposition;
- public configuration excludes local and idle targets, build input carries
  invalid target progress plus exact attempt identity, and swap mode selection
  follows the captured capability;
- predecessor-first connections after secondary restart and primary
  replacement are rejected before and after successor admission; the old
  session's epoch is invalid after the mandatory secondary barrier;
- exact build completion admits a new storage incarnation into the source
  history while the retired incarnation's identity/progress remains fenced;
- PostgreSQL passes the strict public oracle through the new role/epoch/build
  path before any cutover work begins;
- the KSF disposition matrix has a named test and production gate for every
  finding.
- evaluator/executor tests consume only the new public protocol and produce the
  same commands/outcomes expected by the strict conformance oracle.

### Alignment Phase 6A: Build a Parallel Stateless Engine Core

**Status:** delivered in Phase 6. This step created a separate replacement
rather than removing persistence from `DefaultReplicatorInner` incrementally.

- implement a new stateless default Replicator behind a test-only or explicitly
  selected factory;
- add an immutable `LegacyManaged`/`StatelessPreview` process launch selection
  resolved before the host binds a replication listener;
- expose the same selection through public construction/settings, with exactly
  one selected engine and provider writer;
- keep the agent-owned replication server only in legacy mode; in preview mode
  the replacement binds and owns the endpoint;
- abort startup without fallback when replacement Open fails;
- return only the public `ReplicatorInterfaces` bundle;
- create the engine empty on every open and derive progress from
  `StateProvider`/`DurableState`;
- own listener, peer sessions, ACK aggregation, copy,
  replication queues and cancellation internally;
- require controller session renewal, complete surviving-secondary epoch
  barriers, then replay replacement role/primary epoch and finally PC/CC
  configuration through public methods;
- implement `StateReplicator` and provider cooperation through the Phase 2
  contract;
- define role-specific internal progress types, per-peer processing and retry
  ownership in the replacement;
- keep the legacy engine as the production default until the replacement
  acceptance suite passes;
- prove hosted preview endpoint ownership and unchanged production selection.

Exit criteria:

- preview startup creates exactly one engine/provider writer and one
  Replicator-owned listener;
- crossed legacy/preview selection, capability mismatch and preview Open
  failure fail closed without binding or falling back to the legacy engine;
- a replacement process starts unassigned and cannot replay predecessor role,
  PC/CC or peer tokens before higher-epoch controller session renewal;
- the replacement opens no engine metadata store and exposes only the coherent
  public bundle/capabilities;
- role, epoch, configuration, StateReplicator, provider and transport unit
  suites pass against an empty engine.

### Alignment Phase 6B: Complete Recovery, Provider and Semantic Conformance

**Status:** delivered in Phase 6.

- implement durable-before-acknowledgement replication and conservative
  ambiguous client failure without local-write recovery;
- settle applied-above-committed suffixes before serving while publishing
  provider-derived election progress;
- validate exact history after every `on_data_loss` result, including
  `Ok(false)`;
- return `ReplicaRebuildRequired` for incompatible history and permanently
  retire that replica/storage incarnation;
- build empty replacement SQLite and KVStore2 replicas through authorized
  public copy;
- complete public build only after internal copy plus retained replication;
- implement `must_catchup` successor-specific double catch-up with the `All`
  fallback only when specific quorum is unavailable;
- implement SF-style access sequencing around public role, epoch, data-loss,
  configuration and catch-up calls;
- run the exact Phase 2 conformance oracle against both the strict
  public/custom implementation and the built-in preview;
- drive preview failover, swap, process restart, data-loss recovery, build and
  corrective replacement through the real controller evaluator, executor and
  reconciler; direct runtime-command tests do not satisfy this gate;
- pass every required KSF-01 through KSF-15 behavior and prove KSF-16 is
  confined to the unselected legacy engine;
- pass focused bounded ownership, all-survivor restart, divergence, transport,
  stale-session, controller and real-provider conformance tests without
  selecting the replacement for production.

Exit criteria:

- the strict public/custom implementation and built-in preview pass the same
  lifecycle/value/evidence oracle;
- data-loss local callback precedes peer configuration, then exact peer/history
  validation admits the new history or returns rebuild-required;
- all required KSF-01 through KSF-15 tests pass and KSF-16 is reachable only
  through the still-unselected legacy engine;
- real SQLite and KVStore2 fresh-state suites pass under preview selection,
  while PostgreSQL retains independent custom/Public V1 coverage;
- controller-driven preview scenarios match the direct public oracle while
  reading no private progress/build evidence;
- production manifests and the running production protocol remain legacy.

### Alignment Phase 7A: Finalize the Replacement-Only Cutover Release

This is the code-removal portion of alignment Phase 7. It produces the
only binary eligible for production activation; no application traffic uses it
yet.

- remove `ManagedReplicatorLifecycle`, `ManagedReplicatorDataPlane`, managed
  attachments, native observations and private receipts;
- remove the agent-owned default replication RPC dispatch path;
- delete the legacy `DefaultReplicatorInner` persistence/recovery
  implementation;
- delete runtime dependencies on `ManagedReplicaStore`,
  `ReplicationProgressStore`, `LocalWriteJournal` and legacy engine build
  continuation stores;
- move all custom/public build consumers to the Phase 2 agent-owned public
  build store, then delete `BuildAuthorityStore`, `BuildProgressStore` and
  their old tables;
- replace the agent schema and reject all old default/custom stores;
- retain agent-owned authority, effects, removal, switchover and retirement
  state;
- update source guards and documentation so the private managed boundary cannot
  return;
- remove `LegacyManaged` selection and every fallback/crossed-selection path;
- build the deployment artifact with only the stateless factory and
  Replicator-owned transport;
- include the replacement controller, protocol/CRD schema, cutover Lease and
  namespace-scoped RBAC in the same release;
- run all fresh-state conformance/live suites against the exact artifact and
  record its immutable image digest with the KSF gate results.

Exit criteria:

- KSF-16 is closed before activation: no compiled/reachable legacy continuation
  owner, store, capability attachment or replication dispatcher remains;
- source guards reject reintroduction of every removed private path;
- the replacement-only artifact passes the complete Phase 6 acceptance suite
  from empty controller, agent and application state;
- the approved cutover manifest references the exact reviewed/tested artifact
  digest;
- no production deployment has yet selected the new protocol.

### Alignment Phase 7B: Perform the Atomic Fresh-Storage Cutover

This is the offline activation portion of alignment Phase 7.

- require an explicit irreversible-cutover approval recording that required
  application-level exports completed or that no old data is retained;
- disable every application and stop all old runtime processes;
- acquire the exclusive cutover Lease, scale/delete the legacy controller,
  verify zero old controller Pods, revoke its command/signing credential and
  remove its Kubernetes mutation RBAC before changing storage;
- delete or replace all old agent and application storage/PVCs;
- create a fresh installation namespace and controller-owned resource UID; do
  not reset or reuse the old namespace/resource. Quarantine the old namespace
  so already-accepted delayed mutations cannot affect the replacement;
- deploy only the Phase 7A replacement-only controller/runtime/Replicator and
  provider artifact; the new controller uses a fresh protocol generation,
  command credential and namespace-scoped mutation RBAC and must acquire the
  Lease before reconciling;
- initialize fresh controller, authority and provider state;
- build the remaining replicas through exact public build attempts;
- start custom-replicator applications from fresh identity and storage under
  the new public contract;
- reject mixed-version participation and enable application traffic only after
  public recovery completes.

Exit criteria:

- no old process or storage participates after the new protocol is selected;
- the deployed binary has no legacy selection or fallback;
- controller, agent and application all report the fresh protocol/history
  generation;
- a delayed legacy reconcile or command carrying the old resource UID,
  protocol generation, lease identity or credential is rejected without
  recreating old scaffolding;
- a delayed namespaced Kubernetes mutation can affect only the quarantined old
  namespace and never the replacement namespace/resource UID;
- public recovery, build, catch-up and access complete before traffic;
- post-activation ordinary, PostgreSQL smoke and fresh SQLite/KVStore2 live
  gates pass.

No dormant Public V1 or preview step is activated independently in a
production deployment.

### Phase 6 verification boundary

The delivered preview is covered by:

- 13 direct `stateless_preview_recovery` ownership, static/dynamic recovery,
  ambiguity, replacement and dormancy tests;
- the shared KSF runner, which maps built-in preview observations to every
  KSF-01 through KSF-15 result and keeps KSF-16 as a Phase 7-only guard;
- feature-gated controller operation/evidence and session-renewal tests;
- one real SQLite and one real KVStore2 fresh restart/build/corrective-empty-
  replacement journey;
- the unchanged bounded PostgreSQL smoke tier for the independent custom
  implementation.

Every newly added Phase 6 test has an internal bound of at most 270 seconds;
the generic 300-second nextest limit is only an external backstop. Run the
exact selectors serially (`-j 1`): the shared KSF test starts nested Cargo/
nextest work, and the hosted runtime/provider journeys own real listeners and
durable test roots. Parallel execution can add transient build, port and host
resource contention, so it is not the diagnostic oracle. Exact commands and
the documentation-phase verification record are in
[Kuberic Test Strategy](testing.md#service-fabric-public-v1-phase-6-preview).

## Testing Strategy

The implementation must include process-boundary tests, not only in-process
unit tests. The canonical behavior and KSF finding identifiers come from
[Service Fabric Stateful API Semantics and Kuberic Conformance](service-fabric-api-semantics.md).

### Public Lifecycle and API Conformance

- reject primary configuration before Replicator primary role;
- require explicit promotion epoch before application primary role;
- deliver an epoch barrier to a secondary that retains its role across a newer
  epoch;
- verify controller-authorized data-loss `false`, `true`, error and ambiguous
  result handling before access, including history-context replacement and
  invalidation of old evidence;
- crash after provider mutation, renew only a `Convergent` session-scoped
  attempt, and prove changed creation capabilities cannot override the logical
  authorization's original disposition;
- publish and clear application role addresses at exact role completion;
- project only remote active configuration members with exact `must_catchup`
  and progress semantics;
- restart an active secondary, begin replacement admission, race a
  predecessor-primary connection before the newly authorized primary, require
  a new epoch for the replacement primary session, and reject the predecessor
  token both before and after successor authorization;
- reject copy/replication stream access outside secondary role and invalidate
  existing streams on role, epoch, Close and Abort;
- deliver the exact previous-epoch LSN to the provider and verify real public
  replication completion remains pending until both PC and CC quorums commit;
- verify specific-quorum swap ordering, successor inclusion and the `All`
  fallback;
- pass invalid target progress plus exact target/build/session identity to
  build, validate local target state and exclude every in-build target from
  configuration;
- withdraw a build by retiring its exact target process/storage generation;
  reject the old capability after restart and require a new capability for a
  same-epoch replacement;
- bind a successfully built new PVC/storage incarnation to the source history,
  admit it for election without inheriting the discarded PVC's progress floor,
  then fail over and restart all survivors across distinct PVC identities;
- cancel and settle exact build work before removal;
- cancel and drain blocked public callbacks during supersession, Close and
  Abort;
- restart/drop the exact incarnation after transient fault and keep access
  closed;
- verify contained graceful-close failure reaches the selected outer result;
- replay every public callback across crash-after-success-before-journal cuts
  and require its declared convergence, reconciliation or replacement
  disposition;
- run the same assertions against the strict public/custom trace
  implementation, PostgreSQL adapter and built-in preview.

### Restart Reconstruction

- crash before and after replicator creation;
- restart a primary process without changing logical membership, prove it
  remains unassigned/access-closed, rejects predecessor PC/CC and peer sessions,
  then resumes only after controller-issued higher-epoch session renewal and
  completed secondary barriers;
- crash after role replay but before epoch completion;
- crash during PC/CC configuration replay;
- change provider state through `on_data_loss`, re-read progress and rebuild
  volatile queues;
- lose the data-loss result after provider mutation and verify the captured
  `Convergent` or `ReplaceOnAmbiguity` disposition without assuming `false`;
- treat `Ok(false)` as unproven until exact retained history validates;
- require rebuild when post-callback provider history is incompatible;
- verify access remains closed until reconstruction completes;
- verify next LSN starts after state-provider durable progress.

### Writes

- crash before local durable apply;
- crash after local durable apply but before quorum;
- crash after quorum but before caller notification;
- restart with `applied_lsn > committed_lsn` on one and several replicas;
- lose the primary, restart every surviving replica with
  `applied_lsn > committed_lsn`, elect from provider-derived progress and
  settle the suffix before granting access;
- recommit an identical retained suffix after fresh quorum reconstruction;
- reject different bytes at the same LSN and rebuild the divergent replica;
- verify next LSN starts above the applied tail rather than the committed
  boundary;
- verify the caller observes failure or disconnection rather than a fabricated
  success;
- retry by application operation ID and verify no duplicate mutation.

### Builds

- restart source and target at every copy boundary;
- verify the old attempt is fenced;
- verify the new attempt uses a new identity and deterministic snapshot;
- verify public build completion occurs only after internal copy plus retained
  catch-up;
- verify the controller does not require a reported internal build boundary;
- create divergent SQLite history, observe a no-op `on_data_loss`, return
  `ReplicaRebuildRequired`, permanently retire the old replica/PVC, build an
  empty replacement and admit only its fresh healthy session;
- create divergent KVStore2 history, permanently retire the old replica/PVC,
  build an empty replacement and admit only its fresh healthy session;
- verify the old permanent fault cannot be cleared or readmitted after the
  replacement succeeds;
- reject stale, revoked, wrong-epoch and wrong-target build streams without
  changing replacement provider state;
- select a source below the discarded SQLite false tail and verify the empty
  replacement does not retain the old required-progress floor.

### Reconfiguration

- restart during failover, switchover, scale-up, scale-down and retirement;
- verify agent effects recover while the engine starts empty;
- verify stale engine-session outcomes are rejected;
- verify promotion grants access only after public role/epoch/configuration and
  catch-up completion;
- verify revocation blocks new writes before demotion/removal public calls;
- verify PC/CC quorum safety is unchanged.

### Public boundary and reporting

- create the built-in and an application-provided replicator through the same
  public registration path;
- reject any `ManagedReplicatorLifecycle`, `ManagedReplicatorDataPlane`, native
  observation or private receipt attachment in source guards;
- report only public current progress, catch-up capability and durable
  public-call completion;
- verify controller transitions do not read native verified/quorum/committed
  progress or a reported build boundary;
- verify stale successful public calls cannot complete a newer durable
  operation or reopen access.
- verify built-in and custom `StateReplicator` implementations reject new
  writes whenever the public partition write status is not granted.
- verify target copy rejection preserves the typed
  `ReplicaRebuildRequired` classification across wire delivery, source
  `build_replica`, durable operation failure and controller replacement;
- cover disconnect and restart between target fault detection and source
  failure observation.

### Transport

- verify Open binds and returns the Replicator-owned published endpoint;
- reject a peer with the wrong replica identity or predecessor process
  session;
- replace an old peer session after public configuration changes;
- verify Close and Abort terminate the listener, peer sessions, copy streams
  and acknowledgement work;
- verify the agent control/peer-discovery server has no default-replication
  dispatch path after cutover.

### Compatibility

- hold a legacy reconcile immediately before cutover, terminate the old
  controller, revoke its credential/RBAC, activate in a fresh namespace/resource
  UID and prove its delayed real Kubernetes mutation remains confined to the
  quarantined old namespace after the replacement controller acquires the
  Lease;
- reject attempts to attach any pre-cutover agent or application storage;
- initialize fresh agent and provider storage for SQLite, KVStore2 and
  PostgreSQL;
- keep every application disabled throughout the atomic cutover;
- recreate or explicitly reset stale initialized controller status before
  bootstrapping fresh authority;
- verify PostgreSQL and other custom replicators satisfy the new public
  progress, build and typed-error contracts from fresh state;
- verify no mixed-version rollout is accepted accidentally.

## Operational Consequences

- Replicator process restart may restart copy work from the beginning.
- Clients must tolerate ambiguous write completion and use idempotent operation
  identities where exactly-once user behavior is required.
- The agent database remains required and durable.
- Application providers must accurately report durable progress and preserve
  deterministic copy/retained-history behavior.
- Recovery becomes easier to reason about because there are two durable
  owners: agent topology and application state.

## Limitations and Future Work

Strict SF V1 alignment intentionally gives up transparent continuation of
engine-internal work. If Kuberic later requires resumable builds or durable
request completion, those features should be implemented by an explicit
agent-owned workflow or application-owned transaction protocol, not by making
the default Replicator a hidden metadata store again.

The V2 Transactional Replicator model remains a separate possible feature. It
would add a deliberate replicator-level durable log and transaction protocol;
it must not emerge accidentally through incremental exceptions to this
stateless boundary.
