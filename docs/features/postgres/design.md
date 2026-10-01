# PostgreSQL: Replicated PostgreSQL on Kuberic

A PostgreSQL database hosted by the v2 `ReplicaHost`. Its service creates a
`PgReplicator` through the existing `ReplicatorFactory` and returns it from
`StatefulServiceReplica::open`. PostgreSQL owns SQL and WAL; its
`ReplicatorInterfaces` has no operation/copy `StateReplicator` or `StateProvider`.

> **V2 migration status (custom replication):** The existing
> `postgres-replicated` package no longer depends on the classic runtime or
> operator. Supported operations are authorized singleton initialization,
> restart, read/write access, fencing, exact native secondary builds, replay
> catch-up, interrupted-build recovery, failover, planned switchover, sequential
> scale-up, certified secondary removal, replacement and fresh-session recovery.
> The broader
> architecture below remains historical/planned until those phases ship.
> Ordinary application clients use the managed
> `kuberic_app` non-superuser role with `synchronous_commit=remote_apply`;
> clients that deliberately override that durability setting are outside the
> acknowledged-data guarantee. Validation is unit/host-local only and adds no
> KinD dependency.

## SF failover and planned handoff

The existing `Replicator` and `PrimaryReplicator` callbacks remain the only
application extension point. Generic hosting installs exact replica addresses,
sessions, PC/CC and epoch descriptions, journals role/catch-up effects, and
withholds partition write access until the primary-role callback completes.
Neither agent reports nor controller fields contain PostgreSQL policy generations,
receiver receipts or lineage. `GetCurrentProgress` reports the durable WAL end
for ordering, not replay completion; catch-up capability reports the beginning
of locally retained WAL segments. See the
[SF callback reference](../../background/service-fabric/references.md) and
[reconfiguration ordering](../../background/service-fabric/failover.md).
The generic custom-host scalar projections are not PostgreSQL replay receipts;
successful recovery/catch-up callbacks, not those projections, authorize readiness.

PostgreSQL's bounded, authenticated `pgdata.v2` recovery messages validate the
locally installed resource, epoch/configuration, exact sender/receiver identity
and process sessions. The custom replicator durably distributes an invalid
policy before accepting fresh native apply/readback, and does not grant SQL
until every eligible exact standby has accepted the matched policy.

`ChangeRole(Primary)` owns two recovery rounds. A strict `R + W > N` initial
responder set is journaled before receiver drain. Each responder persists its
revocation, removes old-primary connection settings, verifies receiver exit,
retains received/replay progress, and restarts closed. The final round must
match those exact sessions, policy and compatible lineage. Selection orders
received WAL, replay WAL, then exact identity. An unsafe/non-selected candidate
refuses activation; it never treats a scalar rank as recovery proof. The
selected candidate replays the selected boundary, promotes, checkpoints and
revalidates lineage before completing its callback. Surviving exact responders
follow the promoted timeline; former primaries require an authorized rewind or
fresh build and remain stopped in the meantime.

Planned preparation uses the existing two catch-up calls (`All`) around access
revocation. Source demotion checkpoints and persists a handoff intent, performs
verified owned shutdown, and distributes its durable stopped receipt before
returning. Target recovery revalidates that receipt and its replay boundary.
The generic platform persists only the successful SF callback/effect receipt.

All recovery journals use the existing checksummed, generation-serialized store.
Cancellation cannot grant access; replay revalidates installed sessions and
lineage, including a promotion interrupted before metadata publication.
Fresh-session recovery cannot reuse old receiver or election credit. Lost
quorum keeps writes closed. A transaction already accepted locally but waiting
for synchronous acknowledgement has an **unknown outcome** after disconnect;
tests do not claim it rolled back. Writes attempted after a completed stop fence
are definitively rejected and their unique rows remain absent.

API unreachability is not proof that an old primary process was terminated.
Draining an intersecting responder set prevents further supported synchronous
acknowledgements; an explicit completed stop fence proves ordinary and retained
administrative connections are disconnected. Clients intentionally changing
durability settings and independently surviving orphan database processes are
outside the managed acknowledgement contract.

### Host-local failover and restart tests

```sh
cargo test -p postgres-replicated --all-features --test failover -- --test-threads=1
```

The fixture owns its explicit 16 MiB thread stack, following `process_host.rs`;
the command does not require `RUST_MIN_STACK` or other runner overrides.
Planned-handoff tests drop the old service and metadata owner, reopen the same
agent/application files with a fresh session, and reconstruct through the real
agent service. Source cuts cover pre-fence, authority installation, fence intent,
durable shutdown, and completed demotion. Target cuts cover authority installation,
promotion, completed activation, and a granted/acknowledged write. Each cut checks
the exact retained/pending journal, disconnected old clients, continued closure
without fresh admission, and preserved acknowledged rows on a surviving replica.

### Scaling and adversarial local matrix

`testing::PgGroup` drives the existing generic SF authority/build/configuration
effects through real agent stores. It supplies controller decisions, not synthetic
WAL or copy acknowledgements. Scale-up admits only the exact selected completed
build, installs PC/CC before current-only authority, and leaves candidates fenced
until admission. Secondary removal freezes a boundary under closed access and
SF `All`, validates exact-session witnesses, persists the reduced commit, and
records terminal retirement only after application Close. Raw witness progress
never advances custom quorum progress.

Replacement uses a new incarnation/root for the same ordinal. A former primary
can rejoin only through a selected rebuild; rebuilding a configured member uses
the existing repair-build authority. PostgreSQL checkpoints the installed copy's
replay before completion so its timeline remains observable after a receiver
disconnect. Fresh sessions reconnect using app-owned, revalidated source endpoint
and lineage observations. A stopped replica may acknowledge durable policy
metadata, but that acknowledgement is explicitly non-running and cannot earn
recovery quorum credit.

```sh
cargo test -p postgres-replicated --all-features --test reconfiguration --test authority_races --test switchover_checkpoints --test validation_oracles -- --test-threads=1
```

These targets cover sequential 1-to-2-to-3 growth and 3-to-2-to-1 reduction,
same-ordinal replacement, all identities as failover/switchover targets,
quorum loss with readable secondaries, fresh-session restoration, and independent
application/metadata/host reopen at admission boundaries. Oracles compare complete
SQL contents and durable lineage/authority, use unique stale-write IDs, and accept
only closed-connection, shutdown, or physical read-only errors. Syntax errors,
duplicate keys and deadlines are not fencing evidence. Explicit teardown checks
retained pidfds, every allocated listener address and the exact fixture root.

## Running the singleton host

Install local PostgreSQL binaries and run as an unprivileged OS account:

```sh
cargo run -p postgres-replicated -- \
  --resource-uid example --replica-id 1 --pod-uid replica-1 --pvc-uid storage-1 \
  --data-root ./postgres-state --pg-bin /usr/lib/postgresql/16/bin \
  --bearer-token "$KUBERIC_AGENT_BEARER_TOKEN"
```

`--help` lists the equivalent environment variables, PostgreSQL data directory
and port, application metadata directory, control/replication/coordination
listeners, and advertised control/replication endpoints.
`--application-endpoint` (`KUBERIC_APPLICATION_ENDPOINT`) overrides the advertised
custom-replication address when it differs from the local listener.
Optional `--peer-routes`
(`KUBERIC_PEER_ROUTES`) reads a bounded JSON array of exact `identity`, `control`,
and agent `replication` routes. Identities include `replicaId`,
`instanceId`, and `agentGeneration`; duplicate exact routes are rejected. Control
and replication routes belong to the agent; PostgreSQL's coordination endpoint
is advertised by its custom replicator in the exact target's status report.
There is no ordinal-only fallback. Fresh startup waits for authenticated v2
`InitializeAgentStore`, then `EnsureConfiguration` installs exact authority;
a separate write-grant command opens SQL access. Service role notifications
cannot grant access. Established startup validates both agent and application
identity plus PostgreSQL lineage before touching PGDATA, and starts with
external SQL access closed before restoring accepted authority.

Authorized initialization durably binds the application metadata and PGDATA paths
in the agent store before either is created. Creation permission survives an
interrupted first open, but is consumed before the host becomes ready for commands.
Retries may create still-empty storage or reuse a completed `initdb`; partial or
invalid PGDATA is never erased. Every accepted PostgreSQL startup repairs the
managed port/socket/durability settings idempotently and installs closed access
rules before spawning the server. This includes interruption after raw `initdb`
success but before configuration: repair preserves the database identity and data.
Configuration files are replaced atomically so interrupted repairs can be retried.
Once initialized, missing application storage,
changed paths (including changed symlink targets), and absent legacy bindings
permanently reject startup without creating application files. Restoring matching
storage is required; directory emptiness is not permission to reinitialize.

Driver failures await partition fault acknowledgement (bounded to five seconds)
before returning; they do not depend on the cancellable process-monitor queue.
Permanent faults cannot be downgraded within the same runtime. Agent shutdown
persists accepted faults before completion, including rejected/cancelled startup,
and the host waits for that acknowledgement rather than aborting the reporter.
Persistence and shutdown waits are bounded; failure remains an explicit error.
The executable exits nonzero when this durable acknowledgement fails, including
when a signal or coordination listener initiates shutdown. It still drains or
cancels companion tasks and stops PostgreSQL; additional cleanup errors retain
the acknowledgement failure as the primary diagnostic.

Startup runs in an owned task that is joined, not dropped, on SIGINT/SIGTERM.
Cancellation stops either the initialization listener or agent reconstruction,
joins the agent acknowledgement and transport tasks, and closes the application
even if its driver never reached readiness. A signal racing readiness shuts down
and waits for the returned replica instead. Clean early signal cancellation exits
zero; startup, acknowledgement, task-join, or process cleanup failures exit nonzero,
retaining the primary error and additional cleanup diagnostics. The same signal
future spans startup and running operation. The coordination listener is started
only after host readiness, so it cannot terminate first during startup.

On Linux, each run re-executes `postgres-replicated` in an internal, single-threaded
subreaper mode before launching PostgreSQL. It starts in a separate process group,
so terminal-style group SIGINT/SIGTERM reaches the host without killing its
ownership root. The subreaper also catches direct SIGINT/SIGTERM without exiting;
these handlers are confined to the re-exec and reset when launching PostgreSQL.
They do not change the host's handlers or PostgreSQL's explicit shutdown signals.
A socket handshake prevents launch before the owner retains the subreaper's pidfd.
An undispatched run can complete through a socket stop command without signalling
the root; cancellation racing a launch still captures and reaps the launched tree.
This dedicated ownership root adopts and reaps only that launch's descendants,
including double-forked daemons and backends in separate sessions; short-lived
launchers cannot erase ownership.
Readiness validates a live SQL backend and its postmaster parent against this
lineage while parents are stopped. The retained postmaster pidfd, not the
launcher or a mutable PID file, then drives lifetime monitoring. Loss of the
supervisor is also a fault, even while the retained postmaster is still alive.

Shutdown sends fast-stop SIGINT through that postmaster pidfd with a bounded
wait. A missing/foreign PID file remains an explicit error, but is never a
signal target. PID-file-independent cleanup captures the remaining tree, sends
immediate-shutdown signals and escalates to SIGKILL with bounded waits. The
subreaper remains alive to reap orphans and exits only when no children remain.
Successful shutdown requires the supervisor's normal zero exit after `wait`
reports no children, plus pidfd `POLLHUP` for every retained identity (including
the reaped supervisor). Merely observing exit (`POLLIN`) is insufficient because
it includes zombies. Unexpected or signalled supervisor exit is a cleanup failure;
retained postmaster/descendant identities still receive best-effort termination,
but no success claims descendants reparented outside the lost root were reaped.
Before readiness binds the postmaster, unexpected root loss can also orphan
identities the host has not yet captured. This explicitly fails cleanup and
executable completion; neither an early signal nor a successful application close
can hide the retained failure. A healthy root can cancel and reap an unready
launch without a postmaster identity or PID file.
Abort, interrupted startup, cancellation and drop share this cleanup; repeated calls
cannot target a reused PID. `pg_ctl stop` is deliberately not used: it would
reopen the mutable PID file after ownership validation.

The same owned-command mechanism covers `initdb`, `pg_controldata`, readiness and
health `pg_isready` probes, and the existing `pg_ctl promote`, `pg_basebackup` and
`pg_rewind` helpers. Each helper
gets its own registered subreaper before launch, retains ownership while stdout
and stderr are collected concurrently, and returns the launcher's original exit
status over the socket separately from the supervisor's reaping acknowledgement.
Dropping a helper future, including a readiness timeout or reconstruction
cancellation, synchronously terminates and reaps that exact tree. Host abort also
seals the registry against new helper launches and drains it; close after abort
joins cleanup without starting new control-data validation. Helper cleanup errors
remain retained for executable completion, even if the initiating future was
dropped. A successfully cancelled `initdb` and its descendants therefore cannot
resume PGDATA writes after host exit.

Each process run owns a distinct generation of postmaster handles, helper
registry, monitors, lifecycle state and retained cleanup result. Cleanup captures
that generation and its exact helper IDs before executor submission. Cancelling
unstarted cleanup revokes the ticket and restores only that generation's launch
gate/status. Once cleanup starts, it can retire only its captured generation,
even if its waiter disappears. A later lifecycle call can finish that same
retirement on another executor before installing replacement ownership.

Completion publishes a fresh stopped generation only if the captured generation
is still current and explicit abort has not sealed launches. Delayed workers
cannot inspect replacement process/helper slots or overwrite their state/errors.
Startup and abort serialize ownership transfer; old monitors and cancellation
tokens are generation-bound. Fatal cleanup errors remain attached to the failed
generation and prevent replacement rather than being cleared by retry.

Helper outcomes also retain their originating generation through output parsing,
error classification and cleanup. A result from retired work may finish its own
caller, but cannot abort/close a replacement or report Permanent against it.
Error handling takes the lifecycle lock, revalidates that origin, retires only
current ownership and acknowledges the fault before permitting replacement.
Cleanup-result publication uses that same lifecycle barrier; a cancelled
waiter cannot make a stopped replacement generation appear between validation
and acknowledgement. Destructive error actions receive the captured generation,
not a second lookup of the mutable current slot.
Genuine current-generation fatal errors still seal launches and remain fatal;
transient timeout cleanup preserves its existing closed, retryable behavior.

The service's monitor-fault channel carries the generation to the receiving end,
not just to enqueue. Delivery revalidates it under the lifecycle lock before
calling the partition fault API. Queued notices from replaced generations are
discarded. PostgreSQL SQL observation/apply failures use the same origin binding;
the public SF service/replicator API is unchanged.

Retired operations return the existing SF `OperationCancelled` classification
through the outer custom-replicator effect host. The host's effect/configuration
gate serializes authority and session changes, but cancellation is not permission
to abort a successor. Cancelled effects do not publish success or change access;
the agent durably cancels only their matching pending intent. Other failed
current-generation fence postconditions retain fail-closed abort behavior.

Policy invalidation, connection acquisition, each SQL mutation, reload, readback
and durable publication use the captured process lease under the lifecycle
barrier. The shared configuration lock and exact policy version serialize policy
transactions. Retirement may occur between steps, but every following step then
rejects the old lease before acting. SQL steps have bounded deadlines so a stalled
backend cannot indefinitely retain the lifecycle barrier. Policy validity and its
process generation are published by the same joined metadata commit, including postcommit caller
cancellation. A restarted process needs fresh current-configuration apply/readback
before a partition grant can reopen SQL; old metadata alone is not native proof.

Production process generations consume a checksummed 40-byte
`process-generation-v2` high-water mark in the application metadata directory.
Allocation synchronously renames and fsyncs it before publishing ownership, under
the existing exclusive store owner, after startup storage/lineage validation.
Metadata records initialization so loss or
corruption cannot silently reset the clock. A retired manager cannot access a
reopened owner's clock. `u64::MAX` is terminal: exhaustion seals launches, retains
the fatal error, and remains exhausted after reopen. Helper identities and cleanup
epochs are checked within their retained process owner; neither wraps or reopens
an exhausted registry. Policy/metadata increments are checked as well. The agent
reserves its maximum next-effect sequence as a durable exhausted state, rejecting
new effects while retaining exact prior receipts for replay.

Cancellation does not delete partially initialized storage or consume the durable
first-open permission. An authorized retry can initialize still-empty PGDATA or
reuse valid completed `initdb` output with its original system identifier;
nonempty output must pass control-data validation and closed startup, never a
second `initdb` or automatic erasure. Invalid partial output fails closed and
requires storage recovery. These boundaries also apply when cancellation happened
after the launcher exited or before managed configuration was written.

Linux pidfds, child-subreaper support and readable `/proc` are required. Library
hosts must install `postgres-replicated` beside their executable; Cargo integration
tests use the package binary beside `deps/`. There is no extra installed helper
binary and no process-wide subreaper setting in the application host. Abrupt
supervisor SIGKILL and hostile administrators remain outside the guarantee.

Native coordination uses `pgdata.v2` `Build` and `InspectSource`, with the same
bearer credential as the agent. The protobuf messages carry canonical JSON
envelopes capped at 64 KiB. Strict decoding rejects omitted, duplicate, unknown,
noncanonical, oversized, and wrong-version fields. Envelopes bind the resource,
full source/target identities, both agent-created process sessions, immutable
agent build authority, frozen recovery boundary, and bounded system/timeline
history (at most 64 ancestors and 16 KiB of history).

### Acknowledged access fencing

HBA reload plus terminating authenticated application backends is not a fence:
an accepted TCP socket can delay its startup packet and retain pre-reload HBA
rules. Closing previously granted access therefore drains and reaps the entire
owned PostgreSQL run using fast shutdown, then starts it with closed managed HBA
rules before acknowledging the partition-access effect. Every old socket is
dead, including SSL-negotiated connections that have not authenticated. A later
grant cannot revive them. Grant/close operations are serialized, and grants
acknowledge a real application-role login rather than only `pg_reload_conf()`.
Cancellation of an unfinished grant still requires the full closing fence.

This deliberately interrupts existing internal SQL/replication connections.
Their loopback/Unix endpoints reopen, and physical replication reconnects.
Unix sockets live in `PGDATA/pg_stat_tmp`, which backup and rewind exclude;
live sockets must not become input files to a real divergent rewind.
The custom replicator reserves physical WAL slots before build for each exact
replica incarnation, retaining WAL through these reconnects and process restart.
Session-qualified application names remain the acknowledgement gate; a slot is
not quorum credit. The `kuberic_<identity digest>` slot namespace is managed by
the example. Current slots are preserved; consumers of retired slots are drained
before those slots are removed on configuration reconciliation. Operators must budget WAL storage for unavailable
configured replicas. Slot-retention capacity policy is not failover orchestration.
Phase 4 does **not** configure a WAL-byte limit or enforce free-space headroom.
Active admitted replicas and retryable, still-selected builds may therefore
retain unbounded WAL under PostgreSQL's default slot settings. That capacity
policy remains later work; it must not be confused with terminal-slot cleanup.
Retirement drains and drops the slot in one server round trip, with bounded
retries if a receiver reconnects. A slot is never reported retired on
`OBJECT_IN_USE`. Configuration replay retries unfinished cleanup.

`pg_controldata` uses a two-second owned-command deadline. Timeout or caller
cancellation reaps the exact helper tree before returning; cleanup failure is
retained rather than hidden as a retryable timeout. An ordinary helper timeout
is transient, leaves a failed fence closed, and can be retried after the helper
recovers on the same SF service. The transient path joins/stops the exact run
without sealing helper launches; subsequent progress retries can run a fresh
helper and report zero while SQL remains closed. Explicit abort and fatal
ownership/cleanup errors still seal the registry. Malformed control data and
ownership/cleanup failures remain permanent.

### Exact build receipts and catch-up

The private agent store selects one immutable build and durable generation per
logical target slot. Selecting another build or replacement incarnation
durably abandons the old selection. Configuration callbacks describe only that
selected build; incomplete peer-session descriptions confer no build permission.
Private live receipts bind that selection to source/target process sessions and
the local attempt generation. Progress persistence atomically checks the
selection. One scalar observation is never applied to all described builds,
and old scalar journals do not restore current-session completion after a crash.

PostgreSQL keeps active outbound intents separate from suspended retryable work
and compact terminal build-ID tombstones. Superseded/retired descriptions reclaim
active capacity and associated slots; they cannot resurrect on reopen or accept
late completion. A retryable transport cancellation preserves lineage for the
same immutable work but removes its active attempt. Retrying allocates a new
durable attempt generation, so an old RPC cannot publish into that attempt.
The agent retains the full authority/selection audit and immutable operation-ID
bindings. PostgreSQL bounds active plus suspended work to 16, build IDs to 512
bytes, retained IDs to 64 per admitted epoch, and its serialized metadata to
4 MiB. The per-epoch budget reserves space for selected work's eventual
retirement. Exhaustion rejects further distinct work without deleting relevant
IDs; a newer admitted SF epoch is required. Advancing epoch compacts older IDs
into a durable retired-epoch watermark. Requests at/below that watermark remain
rejected after reopen, while current-epoch IDs and exact session/configuration
descriptions reject recent late work. This is epoch compaction, not TTL deletion.

Metadata commits arbitrate cancellation before the rename/publication critical
section. Precommit cancellation is a no-op; after the commit decision wins,
rename/fsync and memory publication finish before cancellation acknowledges.
Workers are synchronously cancelled/joined, not detached. The store retains
exclusive root ownership, and commit locking plus generation comparison prevents
an old owner from consuming or overwriting a reopened owner's staging state.
An I/O failure remains an error; a visible rename is reconciled in memory even
if the following durability step fails.
Catch-up observations take the lifecycle lock only for each probe, avoiding both
stale whole-state publication and an uncancellable lock across the full wait.

Synchronous policy observations derive from committed metadata, not a second
policy cache. A policy-specific version fences the previous valid policy as soon
as invalidation starts; unrelated metadata generations cannot lift that fence.
Invalidation, SQL apply/readback and snapshots are serialized, and the SQL
configuration session holds an advisory lock so a cancelled old query cannot
overtake a later apply. Valid policy is published only after readback, in the same
durable update as its policy version. Cancellation at either publication boundary
therefore cannot republish the preceding valid policy.

`WriteQuorum` uses policy-certified replay. SF `All` additionally requires every
current replica's exact session-qualified WAL receiver to report replay through
the frozen boundary; missing, duplicate or behind receivers cannot satisfy it.
The bounded wait releases the lifecycle lock, and epoch, configuration, access
closure and shutdown cancel it. Joint-configuration transitions remain outside
this phase and are rejected explicitly.

The private agent host installs exact replica/session descriptions through the
SF-shaped replica-set configuration callbacks, never passing an authority store
or admission token to the application. The PostgreSQL replicator serializes
storage mutation with configuration changes and cancels stale work before
installing a new configuration. The agent first admits the target build, then dispatches custom work
instead of operation-stream copy. The receiver revalidates local admission and
the source's persisted exact build/lineage before destructive work. Durable stages
are `Intent`, `Copying`, `Installed`, `Recovering`, and `Complete`. Interrupted
copy/rewind is discarded only under re-admitted authority; installed data is reused.
Reopen never reconnects a previous session: it stays stopped until a fresh exact
source/target session pair resumes the persisted intent.

Matching system identities and compatible timeline histories permit `pg_rewind`.
Fresh, never-admitted initialization is distinguished from established data by
durable authority history, not by comparing LSN magnitudes.
Failure to rewind is explicitly logged and durably switches to a fresh
`pg_basebackup`; an unrelated database or incompatible timeline is rejected,
not ranked by scalar LSN. Rewind uses the internal loopback-only `kuberic_rewind`
role with file-reading privileges, separate from the application role.
The source's validated timeline history is installed before pinned-timeline
recovery. Standby configuration is atomically replaced with the exact
identity/session-derived replication name and external access closed.

Completion requires compatible recovery lineage and both flushed WAL and replay
at or beyond the frozen boundary, including equality. Backup success, process
readiness, WAL receipt, or flush without replay is insufficient. Completion is
persisted before returning structured evidence; only the managed adapter can
publish it into the agent's build journal. Scalar journals do not restore native
completion on restart. Retired authority, changed identities, and replaced process
sessions lose completion credit. The supported crash cuts, same-ordinal replacement,
rewind/fresh fallback, and exact replay boundaries are covered by real host-local
PostgreSQL tests.

All process tests use repository-local directories and local listeners.
The host reserves 16 MiB worker stacks for authority/effect futures,
matching the large agent RPC test fixtures.

### Phase 4 custom-replicator boundary audit

PostgreSQL's custom `PgReplicator` owns the build. Its `build_replica`
callback uses the exact target's advertised replication endpoint, freezes PostgreSQL lineage,
persists application intent, invokes the PostgreSQL RPC, and validates the
returned PostgreSQL recovery certificate. `examples/postgres` alone contains
the `pgdata.v2` protocol, backup/rewind commands, timeline/history compatibility,
replay/flush completion policy, staged metadata, source changes and crash recovery.
The agent neither sends PostgreSQL RPCs nor selects a native recovery method.

The public mapping follows the repository's
[SF references](../../background/service-fabric/references.md):
`StatefulServiceReplica` corresponds to `IStatefulServiceReplica`,
`Replicator` to `IReplicator`, and `PrimaryReplicator` to
`IPrimaryReplicator`. The existing factory selects the service-created
replicator. `StateReplicator` is a separate optional operation/copy capability,
present for the default engine and absent here.

Configuration callbacks install exact incarnation/session/endpoint descriptions
and frozen build boundaries. The target validates its local role, epoch,
configuration and both sessions before backup/rewind. Role notifications do not
grant SQL access: progress observation reconciles the retained partition handle's
read/write statuses and acknowledges completed closure before the agent records
an access effect. Startup config repair always begins externally closed.

The private agent wrapper owns authority stores, effect replay, registration,
peer-session replacement, durable build progress and retirement tombstones.
It rejects stale completion before journaling; reporting does not resurrect
custom build credit from an old scalar journal after process/session replacement.
Build retirement does not stop an already admitted active standby.
`pgdata.v2`, lineage, timelines, WAL receipt/flush/replay and recovery stages
remain exclusively in `examples/postgres`. Shared code contains no database
branches. Protocol 9 carries only the generic replicator endpoint, and schema 5
binds application storage paths/initialization permission.

Shutdown cancels and joins agent and coordination transports instead of waiting
for peers to finish retained HTTP/2 handshakes/streams. The agent still persists
accepted faults before acknowledging shutdown; transport cancellation does not
turn persistence or owned-process cleanup failure into success.
Exhausting the unchanged fast-shutdown grace budget triggers the existing
QUIT/KILL escalation rather than falsely reporting a failed fence after cleanup
was proved. Success still requires normal supervisor completion and reaping of
every descendant. Missing/foreign ownership, non-cooperating launchers,
supervisor loss and incomplete
reaping remain errors; no shutdown sleep or timeout was increased.

---

## Goals

1. PostgreSQL streaming replication managed by kuberic lifecycle
2. Automatic failover: kuberic detects failure, selects best replica
   by LSN, promotes via `pg_ctl promote`
3. Switchover: graceful primary swap with write revocation
4. Epoch fencing: prevent split-brain / zombie primary reads
5. Copy protocol: `pg_basebackup` for new replica builds
6. Demonstrate kuberic as an **orchestration framework**, not just a
   replication engine

## Non-Goals

- Re-implement PostgreSQL streaming replication via kuberic's data plane
- Logical replication (physical streaming only)
- Connection pooling (out of scope — use PgBouncer externally)
- Backup/restore to object storage (CNPG-I concern, not kuberic)
- Multi-primary / active-active writes

---

## Required Configuration

PostgreSQL must be initialized and configured with these settings for
correctness:

```
# Required at initdb time (cannot be changed after)
initdb --data-checksums

# Required runtime settings (postgresql.conf)
wal_log_hints = on              # Enables pg_rewind (belt-and-suspenders with checksums)
synchronous_commit = remote_apply # Writes replayed on sync standbys before client ACK
hot_standby = on                # Allows PgMonitor to query standbys via SQL
logging_collector = off         # Logs go to stderr, piped through tracing
```

**Rationale**:
- `--data-checksums` + `wal_log_hints`: Required for `pg_rewind`. Without
  these, a demoted primary cannot rejoin and must do a full `pg_basebackup`.
- `synchronous_commit = on`: Critical for split-brain safety. If the old
  primary is partitioned, its synchronous commits hang (no standby to ACK),
  providing PG-native write fencing.
- `hot_standby = on`: PgMonitor queries `pg_last_wal_receive_lsn()` on
  standbys via SQL. With `hot_standby = off`, PG rejects all connections
  on standbys and monitoring breaks. Clients don't connect to standbys —
  `hot_standby` is only for monitoring access.

---

## Why Not the WalReplicator?

The kvstore and SQLite examples use kuberic's `WalReplicator` because those
systems have no built-in replication. The replicator ships operations from
primary to secondaries, tracks quorum ACKs, and manages copy/catchup.

PostgreSQL already provides all of this:

| Concern | WalReplicator (kvstore/sqlite) | PostgreSQL Native |
|---------|-------------------------------|-------------------|
| Data shipping | gRPC OperationStream | WAL sender/receiver |
| Durability | User ACKs after fsync | `synchronous_commit` |
| Quorum | QuorumTracker counts ACKs | `synchronous_standby_names ANY N` |
| Copy/rebuild | GetCopyState → user snapshot | `pg_basebackup` (adapter-direct) |
| Catchup | ReplicationQueue replay | Replication slots + WAL retention |
| Rollback | UpdateEpoch → user truncates | No-op (secondaries reconnect via ReconfigureStandby) |

Using WalReplicator for PostgreSQL would mean:
1. Parsing PG's binary WAL format (complex, version-dependent)
2. Bypassing PG's native WAL sender (losing its maturity)
3. Managing WAL segments in two places (PG + kuberic queue)
4. No benefit — PG's streaming is faster and more reliable

**Decision**: Kuberic orchestrates, PostgreSQL replicates.

---

## Architecture

### Integration Pattern: External Replication

This introduces a new integration pattern for kuberic — **external
replication** — where the database handles its own data plane and kuberic
provides the control plane:

```
┌─────────────────────────────────────────────────────┐
│                   kuberic operator                   │
│  (durable failover, switchover, reconfiguration)    │
└──────────────┬───────────────────────┬──────────────┘
               │ gRPC control plane    │
        ┌──────▼──────┐         ┌──────▼──────┐
        │  Pod (P)    │         │  Pod (S)    │
        │             │         │             │
        │ReplicaAgent │         │ReplicaAgent │
        │ PodRuntime  │         │ PodRuntime  │
        │ PgService   │         │ PgService   │
        │ PgMonitor   │         │ PgMonitor   │
        │             │         │             │
        │ ┌─────────┐ │   WAL   │ ┌─────────┐ │
        │ │ postgres ├─┼────────┼─► postgres │ │
        │ │ (primary)│ │streaming│ │(standby) │ │
        │ └─────────┘ │  repl   │ └─────────┘ │
        └─────────────┘         └─────────────┘
```

Key difference from kvstore/sqlite:
- **No WalReplicator actor** — no gRPC data plane between pods
- **No ReplicationQueue** — PG manages WAL retention via replication slots
- **No QuorumTracker** — PG manages `synchronous_standby_names`
- **PgMonitor** replaces replicator — queries PG for LSN, updates
  PartitionState so the operator can make failover decisions

### Client Access & Write Fencing

Clients connect directly to PostgreSQL's TCP port (5432) — there is no
gRPC proxy intercepting queries. This means kuberic's
`PartitionState.write_status()` cannot gate client writes at the
application layer like kvstore/sqlite do.

**How kvstore/sqlite fence writes**: `revoke_write_status()` sets an
atomic (`AccessStatus::ReconfigurationPending`), and
`StateReplicatorHandle::replicate()` checks this atomic before accepting
data — returning `Err(ReconfigurationPending)` immediately. This works
because all writes go through kuberic's WalReplicator data plane.

**Why PG is different**: PG clients connect directly to PG's TCP port.
Writes go through PG's native SQL engine, never touching kuberic's
`replicate()` or `write_status()` atomic. The atomic is set but nobody
checks it.

Instead, write fencing uses **PostgreSQL's native mechanisms**:

| Kuberic Event | PG Fencing Action | Effect on Clients |
|---------------|-------------------|-------------------|
| ChangeRole(ActiveSecondary) — demotion | `ALTER SYSTEM SET default_transaction_read_only = on` + `pg_reload_conf()` | New transactions get `ERROR: cannot execute ... in a read-only transaction` |
| ReconfigureStandby — reconnect to new primary | Create `standby.signal` + restart as standby | PG is physically read-only (standby mode) |
| Epoch fence (zombie primary) | Shut down PG → rejoin as standby via BuildReplica | Connections dropped |
| ChangeRole(Primary) — promotion | `pg_ctl promote` + `ALTER SYSTEM SET default_transaction_read_only = off` + reload | Writable |

**Implementation hook**: The adapter's `ChangeRole(ActiveSecondary)`
handler is the demote signal. During switchover, the operator submits a
correlated `RevokeWriteStatus` action and then a correlated
`ChangeRole(new_epoch, ActiveSecondary)` action to the old primary.
The adapter executes the PG-level fencing there:

```rust
// adapter.rs — handle_change_role
Role::ActiveSecondary => {
    // If demoting from Primary → fence writes at PG level
    if current_role == Role::Primary {
        if let Ok((client, _conn)) = instance.connect().await {
            // Soft fence: new transactions default to read-only
            let _ = client
                .execute("ALTER SYSTEM SET default_transaction_read_only = on", &[])
                .await;
            let _ = client.execute("SELECT pg_reload_conf()", &[]).await;
            info!("write fencing applied: default_transaction_read_only = on");
        }
    }
    monitor.set_role(Role::ActiveSecondary);
    Ok(())
}
```

**Write fence lifecycle**:

```
Switchover timeline on old primary:

1. revoke_write_status()
   └─ atomic set to ReconfigurationPending (PG doesn't know yet)

2. ChangeRole(ActiveSecondary)                    ← adapter fences here
   └─ ALTER SYSTEM SET default_transaction_read_only = on
   └─ pg_reload_conf()
   └─ New transactions get read-only errors immediately

3. UpdateCatchUpConfiguration
   └─ ReconfigureStandby RPC arrives
   └─ Create standby.signal (KP-3 fix)
   └─ Rewrite primary_conninfo → new primary
   └─ Restart PG as physical standby              ← hard fence
```

**Removing the fence on promotion**: When a standby is promoted to
primary, `ChangeRole(Primary)` should also clear the fence:

```rust
Role::Primary => {
    // ... promote logic ...
    // Clear write fence if previously set
    if let Ok((client, _conn)) = instance.connect().await {
        let _ = client
            .execute("ALTER SYSTEM SET default_transaction_read_only = off", &[])
            .await;
        let _ = client.execute("SELECT pg_reload_conf()", &[]).await;
    }
    monitor.set_role(Role::Primary);
    Ok(())
}
```

**Limitations of `default_transaction_read_only`**:

- **Bypassable**: Clients can override with `SET SESSION
  default_transaction_read_only = off` or `BEGIN READ WRITE`. This is a
  known PG limitation — the setting is a default, not a restriction.
- **Not instant for existing transactions**: In-flight write transactions
  complete; only new transactions are affected.
- **Adequate for kuberic's switchover window**: The window between
  demotion and PG restart (ReconfigureStandby) is typically <2 seconds.
  Combined with `synchronous_commit = on` (writes hang when standbys
  disconnect), the risk of data divergence is minimal.
- **Hard fence follows**: PG is restarted as a standby (physically
  read-only) within seconds. The soft fence just buys time.

**Client routing** uses a Kubernetes Service with label selectors:
- **Read-write Service** (`-rw`): selects only the pod with
  `role=primary` label. Kuberic operator updates pod labels on
  role changes.
- **Read-only Service** (`-ro`, Phase 2): selects pods with
  `role=secondary` + `hot_standby=on`.

Clients connect to the Service DNS name (e.g.,
`mydb-rw.namespace.svc:5432`). On failover, the operator relabels pods
and the Service automatically routes to the new primary — no client-side
discovery needed.

**PartitionState still tracks access status** — PgMonitor updates
`read_status` and `write_status` atomics based on PG's actual state
(e.g., `pg_is_in_recovery()`, `default_transaction_read_only`). The
operator uses these for health/status reporting, but they don't gate
client access — PG does that itself.

### Component Overview

| Component | Responsibility |
|-----------|---------------|
| **PgService** | Lifecycle event handler. Starts/stops PG, handles ChangeRole |
| **PgMonitor** | Polls PG for replication status, updates PartitionState |
| **PgReplicatorAdapter** | Implements ReplicatorHandle contract; delegates to PgMonitor |
| **PgInstanceManager** | PG process management: start, stop, promote, configure |

---

## Detailed Design

### PgInstanceManager — PostgreSQL Process Lifecycle

Wraps `pg_ctl` and PG configuration. Runs PostgreSQL as a child process.

```rust
pub struct PgInstanceManager {
    data_dir: PathBuf,
    pg_bin: PathBuf,         // e.g. /usr/lib/postgresql/16/bin (CLI arg)
    child: Option<Child>,    // PostgreSQL server process
    port: u16,
    socket_dir: PathBuf,     // UDS directory (= data_dir for isolation)
}

impl PgInstanceManager {
    /// Initialize a new PG cluster (initdb --data-checksums)
    pub async fn init_db(&self) -> Result<()>;

    /// Start PostgreSQL with given config.
    /// Passes -c unix_socket_directories=<socket_dir> -c port=<port>.
    pub async fn start(&mut self, config: &PgConfig) -> Result<()>;

    /// Stop PostgreSQL (fast mode)
    pub async fn stop(&mut self) -> Result<()>;

    /// Promote standby to primary (pg_ctl promote)
    pub async fn promote(&self) -> Result<()>;

    /// Run pg_basebackup from a source to initialize this replica
    pub async fn base_backup(&self, source_addr: &str) -> Result<()>;

    /// Run pg_rewind to rejoin as standby after demotion
    pub async fn rewind(&self, target_addr: &str) -> Result<()>;

    /// Configure streaming replication (primary_conninfo, etc.)
    pub fn configure_standby(&self, primary_addr: &str) -> Result<()>;

    /// Configure synchronous standbys
    pub fn configure_sync_standbys(&self, standbys: &[String]) -> Result<()>;

    /// Connect to the local PG instance
    pub async fn connect(&self) -> Result<PgConnection>;
}
```

### PostgreSQL Log Handling

PostgreSQL writes all log output to **stderr** (`logging_collector = off`).
Stdout is silent during normal operation. The instance manager pipes both
through `tracing`:

```rust
// In PgInstanceManager::start()
let mut child = Command::new(self.pg_bin.join("postgres"))
    .args(["-D", &self.data_dir.to_string_lossy()])
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .spawn()?;

let stdout = BufReader::new(child.stdout.take().unwrap());
let stderr = BufReader::new(child.stderr.take().unwrap());
tokio::spawn(async move {
    let mut stdout_lines = stdout.lines();
    let mut stderr_lines = stderr.lines();
    loop {
        tokio::select! {
            Ok(Some(line)) = stderr_lines.next_line() => {
                tracing::info!(target: "postgres", "{}", line);
            }
            Ok(Some(line)) = stdout_lines.next_line() => {
                tracing::debug!(target: "postgres", "{}", line);
            }
            else => break,
        }
    }
});
```

This gives unified structured logging — PG messages appear alongside
kuberic's own traces, filterable by `target: "postgres"`. In K8s,
container stdout is collected by the log aggregator as usual.

**Note**: CNPG uses a more sophisticated approach — PG writes CSV to a
FIFO via `logging_collector = on`, and a LogPipe goroutine re-serializes
to JSON on stdout. That's overkill for this example.

### Child Process Monitoring

The PG child process must be monitored for unexpected exit (OOM kill,
segfault, disk corruption). A separate task awaits `child.wait()` and
reports fault on unexpected exit:

```rust
// In PgInstanceManager::start(), after spawning log forwarder:
let fault_tx = fault_tx.clone();
tokio::spawn(async move {
    let status = child.wait().await;
    match status {
        Ok(exit) if exit.success() => {
            tracing::info!(target: "postgres", "PostgreSQL exited normally");
        }
        Ok(exit) => {
            tracing::error!(target: "postgres", "PostgreSQL exited: {}", exit);
            let _ = fault_tx.send(FaultType::Permanent).await;
        }
        Err(e) => {
            tracing::error!(target: "postgres", "Failed to wait on PG: {}", e);
            let _ = fault_tx.send(FaultType::Permanent).await;
        }
    }
});
```

On unexpected exit, `fault_tx` notifies the operator, which triggers
failover. This is critical because PG dying inside a running pod does
not make the pod NotReady unless a liveness probe detects it.

### PgMonitor — Replication Status Polling

Periodically queries PostgreSQL for replication progress and updates
kuberic's `PartitionState` atomics. This is how the operator knows
each replica's LSN for failover decisions.

#### PostgreSQL WAL LSN Pipeline

The primary tracks every standby's progress via `pg_stat_replication`:

```
Primary WAL pipeline (per standby):

  pg_current_wal_lsn()  →  sent_lsn  →  write_lsn  →  flush_lsn  →  replay_lsn
  (WAL generated)          (sent to      (written to    (fsync'd on    (applied on
                            standby)      standby disk)  standby disk)  standby DB)
```

| Column | Meaning | Source |
|--------|---------|--------|
| `pg_current_wal_lsn()` | Latest WAL position on primary | Primary (SQL function) |
| `sent_lsn` | How far WAL sent to each standby | Primary (`pg_stat_replication`) |
| `flush_lsn` | How far standby has fsync'd | Primary (`pg_stat_replication`) |
| `replay_lsn` | How far standby has applied | Primary (`pg_stat_replication`) |

**Key insight**: The primary already knows every standby's progress.
No need to query standbys directly for catchup or quorum tracking.
`flush_lsn` (not `replay_lsn`) is the durability boundary for
`synchronous_commit = on`.

#### PgMonitor Implementation

```rust
pub struct PgMonitor {
    instance: Arc<PgInstanceManager>,
    state: Arc<PartitionState>,
    fault_tx: mpsc::Sender<FaultType>,
    role: Role,
    consecutive_failures: u32,
}

impl PgMonitor {
    /// Run the monitor loop. Polls PG every 1 second.
    pub async fn run(&self, token: CancellationToken) {
        let interval = Duration::from_secs(1);
        loop {
            tokio::select! {
                _ = token.cancelled() => break,
                _ = tokio::time::sleep(interval) => {
                    self.poll_status().await;
                }
            }
        }
    }

    /// Force an immediate poll. Called by the operator before failover
    /// candidate selection to minimize LSN staleness.
    pub async fn poll_now(&self) {
        self.poll_status().await;
    }

    async fn poll_status(&self) {
        match self.role {
            Role::Primary => {
                // current_progress:
                //   SELECT pg_current_wal_lsn()
                //
                // committed_lsn (for ANY N quorum):
                //   SELECT flush_lsn FROM pg_stat_replication
                //   WHERE sync_state IN ('sync', 'quorum')
                //   ORDER BY flush_lsn DESC
                //   LIMIT 1 OFFSET (N-1)
                //   → Nth-highest flush_lsn = quorum durability boundary
                //
                // On query failure: retain last-known-good values, increment
                //   consecutive_failures. After 3 failures → fault_tx.
            }
            Role::ActiveSecondary | Role::IdleSecondary => {
                // current_progress:
                //   SELECT pg_last_wal_receive_lsn()
                //   → self-reported receive position (for operator failover selection)
                //   Returns NULL if never connected — retain previous value.
                //
                // On query failure: retain last-known-good values.
            }
            _ => {}
        }
    }
}
```

**Staleness note**: PgMonitor polls every 1 second. For failover candidate
selection, the operator should call `poll_now()` on all reachable replicas
before reading `PartitionState` to minimize staleness. Under synchronous
replication, replicas are typically within milliseconds of each other, so
1-second staleness rarely changes candidate ranking.

**LSN type mapping**: PostgreSQL's `XLogRecPtr` is a 64-bit byte offset
(text format `0/XXXXXXXX`). Parse as `u64`, cast to kuberic's `Lsn` (`i64`).
Comparison semantics (>, <, ==) are preserved within the same timeline.
Cross-timeline LSN comparisons are invalid — PgMonitor must invalidate
stale LSN values on epoch change (reset to 0 and re-query).

The PartitionState updates enable:
- Operator reads `current_progress` for failover candidate selection
- Operator reads `committed_lsn` for quorum health assessment
- `read_status` / `write_status` fencing works as normal

### PostgreSQL Connection Strategy

PgMonitor and PgInstanceManager connect to the local PG instance via
**Unix domain socket** (UDS) — same approach as CNPG. No TCP overhead,
no TLS, `peer` auth (no password):

```rust
// Connect via UDS — socket_dir is the data_dir for test isolation
let (client, conn) = tokio_postgres::connect(
    &format!("host={} port={} user=postgres dbname=postgres",
             self.socket_dir.display(), self.port),
    NoTls,
).await?;

// Spawn the connection task
tokio::spawn(conn);
```

**Dependency**: `tokio-postgres` (async PG client). Used for all SQL
queries (monitoring, fencing, health checks). Admin tools (`pg_ctl`,
`initdb`, `pg_basebackup`, `pg_rewind`) are spawned as child processes.

### Test Instance Isolation

Replication/failover tests run 2–3 PG instances simultaneously on the
same host. Each instance uses its **data_dir as the socket directory**
to avoid collisions:

```
Instance 1: port=15432, data_dir=/tmp/pg-test-1, socket_dir=/tmp/pg-test-1
Instance 2: port=15433, data_dir=/tmp/pg-test-2, socket_dir=/tmp/pg-test-2
Instance 3: port=15434, data_dir=/tmp/pg-test-3, socket_dir=/tmp/pg-test-3
```

PG is started with `-c unix_socket_directories=<data_dir>` so the socket
file (`.s.PGSQL.<port>`) lives inside the data directory. Cleanup is just
`rm -rf data_dir`. Tests use `#[serial]` to avoid port contention across
test cases (same as kvstore/sqlite).

### PgReplicatorAdapter — Framework Integration

kuberic-core's PodRuntime requires a `ReplicatorHandle` from the `Open`
lifecycle event. The adapter satisfies this contract without running a
WalReplicator. It directly receives `ReplicatorControlEvent` on the
control channel and handles each variant with PG-specific logic:

```rust
/// Spawn the PG control event loop. Returns a ReplicatorHandle
/// that the runtime uses to send lifecycle commands.
pub fn create_pg_replicator(
    instance: Arc<PgInstanceManager>,
    state: Arc<PartitionState>,
    monitor: Arc<PgMonitor>,
) -> ReplicatorHandle {
    let (control_tx, mut control_rx) = mpsc::channel::<ReplicatorControlEvent>(16);
    let shutdown = CancellationToken::new();
    let data_address = instance.replication_address(); // pod IP:port, not localhost

    let mut current_role = Role::None;

    tokio::spawn(async move {
        while let Some(event) = control_rx.recv().await {
            match event {
                ReplicatorControlEvent::Open { reply, .. } => {
                    // PG already started in LifecycleEvent::Open
                    let _ = reply.send(Ok(()));
                }
                ReplicatorControlEvent::ChangeRole { epoch, role, reply } => {
                    if role == current_role {
                        let _ = reply.send(Ok(())); // idempotent
                        continue;
                    }
                    // pg_ctl promote (if Primary), stop PG (if None),
                    // configure standbys, update monitor role
                    current_role = role;
                    let _ = reply.send(Ok(()));
                }
                ReplicatorControlEvent::UpdateEpoch { epoch, reply } => {
                    // pg_rewind directly if diverged (no state_provider needed)
                    let _ = reply.send(Ok(()));
                }
                ReplicatorControlEvent::BuildReplica { replica, reply } => {
                    // Coordinate pg_basebackup via state provider
                    let _ = reply.send(Ok(()));
                }
                ReplicatorControlEvent::UpdateCatchUpConfiguration { reply, .. } => {
                    // Map to PG: add replica to synchronous_standby_names
                    // ALTER SYSTEM SET synchronous_standby_names = '...'
                    // SELECT pg_reload_conf()
                    let _ = reply.send(Ok(()));
                }
                ReplicatorControlEvent::UpdateCurrentConfiguration { reply, .. } => {
                    // Finalize sync standby list after catchup complete
                    let _ = reply.send(Ok(()));
                }
                ReplicatorControlEvent::WaitForCatchUpQuorum { reply, .. } => {
                    // "Caught up" = standby flush_lsn >= target_lsn
                    // All data queried from PRIMARY's pg_stat_replication.
                    let inst = instance.clone();
                    tokio::spawn(async move {
                        let target_lsn = inst.query_current_wal_lsn().await;
                        loop {
                            // SELECT flush_lsn FROM pg_stat_replication
                            // WHERE application_name IN (must_catch_up replicas)
                            // All flush_lsn >= target_lsn? → done
                            tokio::time::sleep(Duration::from_millis(500)).await;
                            if all_caught_up { break; }
                            // Timeout after 30s → reply Err
                        }
                        let _ = reply.send(Ok(()));
                    });
                }
                ReplicatorControlEvent::RemoveReplica { reply, .. } => {
                    // Remove from synchronous_standby_names + reload
                    let _ = reply.send(Ok(()));
                }
                ReplicatorControlEvent::OnDataLoss { reply } => {
                    let _ = reply.send(Ok(DataLossAction::default()));
                }
                ReplicatorControlEvent::Close { reply } => {
                    // pg_ctl stop -m fast
                    let _ = reply.send(Ok(()));
                }
                ReplicatorControlEvent::Abort => {
                    // pg_ctl stop -m immediate (best effort)
                }
            }
        }
    });

    ReplicatorHandle::new(control_tx, state, data_address, shutdown)
}
```

The adapter creates a `ReplicatorHandle` with:
- `control_tx` → receives `ReplicatorControlEvent` directly (no wrapper enum)
- `state` → shared PartitionState (updated by PgMonitor)
- `data_address` → PG's streaming replication address (pod IP:port from
  `OpenContext.data_bind`, not localhost — must be routable from peers)
- `shutdown` → cancellation token

**Key**: The adapter does NOT create a gRPC data server. The
`data_address` points to PostgreSQL's native replication port.

### Open Sequence

```
LifecycleEvent::Open { ctx }
  │
  ├─ Create PgInstanceManager
  │   └─ If OpenMode::New → initdb + configure
  │   └─ If OpenMode::Existing → verify data_dir
  │
  ├─ Start PostgreSQL
  │   └─ pg_ctl start -D data_dir
  │
  ├─ Create PgMonitor
  │   └─ Spawn polling loop (updates PartitionState)
  │
  ├─ Create PgReplicatorAdapter
  │   └─ Spawn control event handler (handles all lifecycle directly —
  │      no StateProvider channel needed, PG operations are inline)
  │   └─ Build ReplicatorHandle
  │
  └─ Reply with ReplicatorHandle
```

### ChangeRole Flows

#### → Primary

```
ChangeRole { role: Primary }
  │
  ├─ If was standby → pg_ctl promote -w -t 60
  │   └─ Wait for promotion: poll pg_is_in_recovery() = false
  │   └─ Timeout after 60s → reply error (triggers switchover rollback A3)
  │
  ├─ Configure synchronous standbys
  │   └─ ALTER SYSTEM SET synchronous_standby_names = 'ANY 1 (...)'
  │   └─ SELECT pg_reload_conf()
  │
  ├─ Update PgMonitor role → Primary
  │   └─ Monitor now queries pg_stat_replication
  │
  ├─ Start client gRPC server (idempotent — skip if already running)
  │   └─ Accept SQL queries from clients
  │
  └─ Reply with client address
```

#### → IdleSecondary (new replica join)

```
ChangeRole { role: IdleSecondary }
  │
  ├─ Ensure PG is stopped
  │
  ├─ Update PgMonitor role → IdleSecondary
  │
  └─ Reply immediately with empty address
      (BuildReplica arrives later as a separate control event —
       do NOT block waiting for it here)
```

#### → ActiveSecondary (after copy + catchup)

```
ChangeRole { role: ActiveSecondary }
  │
  ├─ Verify PG is running and streaming
  │   └─ Check pg_last_wal_receive_lsn() is advancing
  │
  ├─ Update PgMonitor role → ActiveSecondary
  │
  └─ Reply with empty address
```

#### → None (demotion)

```
ChangeRole { role: None }
  │
  ├─ Set write_status → NotPrimary
  ├─ PG keeps running (stopped on Close, not here)
  └─ Reply OK
```

**Data lifecycle**: ChangeRole(None) marks the role. Close does cleanup:
- **ChangeRole(None) → Close**: PG stopped. Data directory deleted.
- **Close (from any other role)**: PG stopped. Data directory preserved.
  The replica will be reopened with `OpenMode::Existing`.

### Copy Protocol — pg_basebackup

When the operator builds a new replica, `BuildReplica` is sent to the
**primary's** adapter (kuberic convention), with the secondary's
`replicator_address`. In WalReplicator, the primary connects to the
secondary's gRPC data server and pushes state. For PG, we use the same
app-to-app `data_address` channel but with a PG-specific gRPC service.

#### PgDataService — App-to-App Protocol

Each PG pod runs a `PgDataService` gRPC server on `data_bind` (the same
address that WalReplicator would use for `ReplicatorDataServer`). This
keeps the existing two-address model:

| Address | Owner | Purpose |
|---------|-------|---------|
| `control_bind` | ControlServer | Operator → pod lifecycle |
| `data_bind` | PgDataService | Pod → pod (BuildReplica coordination) |

```protobuf
service PgDataService {
    // Primary calls this on the secondary during BuildReplica.
    // Secondary runs pg_basebackup from the given primary address,
    // configures standby.signal, and starts PG.
    rpc CloneFrom(CloneFromRequest) returns (CloneFromResponse);
}

message CloneFromRequest {
    string primary_host = 1;
    uint32 primary_port = 2;
}

message CloneFromResponse {
    bool success = 1;
    string error = 2;
}
```

#### BuildReplica Flow

```
Operator: add_replica(secondary_handle)
  │
  ├─ 1. Open secondary → initdb + start PgDataService on data_bind
  │
  ├─ 2. ChangeRole(IdleSecondary) on secondary → reply immediately
  │
  ├─ 3. BuildReplica(secondary_info) on PRIMARY
  │      │
  │      ├─ Primary adapter receives BuildReplica
  │      │   └─ replica_info.replicator_address = secondary's data_bind
  │      │
  │      ├─ Primary connects to secondary's PgDataService
  │      │   └─ Calls CloneFrom { primary_host, primary_port }
  │      │
  │      ├─ Secondary handles CloneFrom:
  │      │   ├─ Stop PG, remove data_dir contents
  │      │   ├─ pg_basebackup -D data_dir -h primary_host -p primary_port -R
  │      │   │   (-R creates standby.signal + primary_conninfo)
  │      │   ├─ Write kuberic config (socket_dir, port, etc.)
  │      │   ├─ Start PG → auto-streams from primary
  │      │   └─ Reply CloneFromResponse { success: true }
  │      │
  │      └─ Primary replies Ok on BuildReplica reply channel
  │
  ├─ 4. ChangeRole(ActiveSecondary) on secondary
  │
  └─ 5. Reconfigure quorum
```

**Failure handling**: If `pg_basebackup` fails (network timeout, disk
full), the secondary cleans up the partial data directory and replies
`CloneFromResponse { success: false, error: "..." }`. The primary
translates this to an error on the `BuildReplica` reply channel. The
operator retries with backoff or marks the replica as failed.

### Epoch Handling

`UpdateEpoch` is sent to surviving secondaries after failover. For PG,
this is a **no-op** — no pg_rewind or divergence check needed.

**Why no-op is correct (contrast with kvstore/sqlite)**:

In the WalReplicator path, `UpdateEpoch` forwards to
`StateProviderEvent::UpdateEpoch { epoch, previous_epoch_last_lsn }`.
kvstore and sqlite use this to **rollback uncommitted ops** — operations
that were applied locally (`current_lsn > previous_epoch_last_lsn`) but
never quorum-committed by the old primary. Without rollback, the
secondary would have divergent state.

PG doesn't need this because:

1. **PG handles divergence natively.** When a secondary reconnects to a
   new primary (via `ReconfigureStandby`), PG's timeline following
   mechanism automatically replays WAL from the fork point. No manual
   rollback is possible or needed — PG's WAL receiver is the authority.

2. **No in-memory state to rollback.** kvstore/sqlite have in-memory
   ops applied by the user's `StateProvider`. PG's WAL replay is
   entirely internal to the postgres process — the adapter has no
   ops to undo.

3. **`previous_epoch_last_lsn` doesn't map.** The framework's
   `committed_lsn()` is observational (PgMonitor polls
   `pg_stat_replication`), not the authoritative PG WAL boundary.
   Rolling back to this value would be meaningless for PG.

**What happens to each replica type during failover**:

- **Zombie primaries** are excluded by the durable failover workflow — they
  never receive `UpdateEpoch`.
- **Surviving secondaries** get `ReconfigureStandby` (via
  `UpdateCatchUpConfiguration`) to reconnect to the new primary. PG
  handles timeline following automatically when reconnected.
- **Demoted primaries** (switchover) get `ReconfigureStandby` which
  detects `was_primary` and uses `pg_rewind` (fallback: `pg_basebackup`)
  to handle timeline divergence.

**Minor consideration**: PgMonitor's cached LSN values may be briefly
stale after epoch change (from old timeline). LSNs are monotonic across
PG timelines in practice (new timeline continues from fork point), so
this doesn't affect failover candidate selection. A future optimization
could reset `current_progress` to 0 in UpdateEpoch and let PgMonitor
re-query on the next poll cycle.

### Failover Sequence

```
1. Operator detects primary failure (gRPC health check fails)

2. Operator triggers poll_now() on all reachable replicas
   └─ PgMonitor does immediate LSN query (minimizes staleness)

3. Operator reads PartitionState from each replica
   └─ current_progress = pg_last_wal_receive_lsn (freshly polled)

4. Select best candidate: highest current_progress
   └─ Tie-break: replica ID (deterministic)

5. ChangeRole(Primary) on selected replica
   └─ pg_ctl promote -w -t 60
   └─ Configure sync standbys
   └─ Start client server

6. ChangeRole(None) on old primary (if reachable)
   └─ Stop PG entirely (pg_ctl stop -m fast)
   └─ Not just write revocation — PG must be fully stopped

7. UpdateEpoch on remaining secondaries (no-op — secondaries already
   reconnected via ReconfigureStandby in step 5's UpdateCatchUpConfig)

8. Old primary eventually gets BuildReplica
   └─ pg_basebackup → rejoin as standby
```

### Switchover Sequence

```
1. Operator initiates switchover to target replica

2. Revoke write status on old primary
   └─ PartitionState atomic set (PG-level fencing is KP-4 future work)

3. ChangeRole(ActiveSecondary) on old primary
   └─ Monitor role updated, PG still running

4. ChangeRole(Primary) on target
   └─ pg_ctl promote -w -t 60 (creates timeline N+1)
   └─ Configure sync standbys

5. UpdateCatchUpConfiguration on new primary
   └─ ReconfigureStandby on old primary:
      - Detects was_primary (no standby.signal)
      - Tries pg_rewind from new primary (fast — diverged pages only)
      - If pg_rewind fails (WAL recycled): full pg_basebackup fallback
      - Creates standby.signal, patches config, starts as standby
   └─ ReconfigureStandby on other standbys:
      - Rewrites primary_conninfo to new primary
      - Restarts PG (timeline following automatic)
   └─ Configures synchronous_standby_names (ANY {quorum})

6. WaitForCatchUpQuorum on new primary
   └─ Polls flush_lsn from pg_stat_replication until standbys caught up

7. UpdateCurrentConfiguration (finalize)
```

**Timeline divergence during switchover**: Between steps 3 and 5, the
old primary's PG is still running and its background processes
(checkpointer, stats) generate small WAL records on the old timeline.
Other standbys connected to the old primary receive these records,
pushing their recovery point past the fork point. When reconfigured to
follow the new primary's timeline, they can't follow without rewinding.

For the **old primary** (was_primary = true): `ReconfigureStandby`
detects this and uses pg_rewind (or pg_basebackup fallback).

For **other standbys**: The divergence is typically tiny (< 1 WAL
segment from background PG activity). PG can usually follow the new
timeline automatically because the divergence is within the same WAL
segment that the new primary also has. In rare cases where the standby
has truly diverged past the fork point, the restart will fail and the
operator will rebuild via BuildReplica/CloneFrom on the next reconcile
cycle.

**Contrast with failover**: In failover, the old primary is dead — all
standbys' WAL receivers disconnect immediately. Their recovery points
stay at or before the fork point, so timeline following works cleanly
without pg_rewind.

---

## Comparison with CNPG

| Aspect | CNPG | Kuberic PostgreSQL |
|--------|------|--------------------|
| **Operator** | Go, full K8s operator with CRD | Rust, CRD-backed durable workflows |
| **Instance manager** | Go binary (PID 1 in pod) | Rust PgInstanceManager (child process) |
| **Failover trigger** | HTTP health check failure | gRPC control plane failure |
| **Candidate selection** | LSN-based (received, then replayed) | LSN-based (PartitionState.current_progress) |
| **Promotion** | `pg_ctl promote` | `pg_ctl promote` |
| **Demoted primary** | `pg_rewind` (automatic) | `pg_rewind` first, `pg_basebackup` fallback (via ReconfigureStandby) |
| **Fencing** | Annotation-based + self-fencing | Epoch-based (kuberic protocol) |
| **Quorum** | PG `synchronous_standby_names` | PG `synchronous_standby_names` |
| **Split-brain** | Operator detects multi-primary | Epoch fencing + PG sync commit fencing |
| **Pod management** | Direct pod management (no StatefulSet) | kuberic-operator manages pods |
| **New replica** | `pg_basebackup` | `pg_basebackup` (via copy protocol) |
| **WAL archiving** | Plugin-based (CNPG-I) | Not implemented (future) |

Key differences:
1. **Fencing**: CNPG uses annotation-based fencing + liveness probe
   self-fencing. Kuberic uses epoch-based fencing (SF protocol) — simpler,
   distributed after promotion, prevents zombie reads atomically.
2. **Operator model**: CNPG manages pods directly. Kuberic persists durable
   topology workflows in CRD status and executes fenced actions through each
   pod's ReplicaAgent.
3. **Instance manager**: CNPG's is a full Go binary running as PID 1.
   Kuberic's is a Rust library called by PgService — lighter weight
   but requires the kuberic runtime as the process entry point.

---

## Framework Impact

This example introduces the **external replication** pattern. Changes
needed in kuberic-core:

### Required Changes

1. **`ReplicatorHandle` generalization**: Currently tightly coupled to
   WalReplicatorActor. The handle already uses a generic control channel
   (`mpsc::Sender<ReplicatorControlEvent>`) — no change needed, but the
   PgReplicatorAdapter must translate `ReplicatorControlEvent` to PG
   operations.

2. **Data address semantics**: Currently `data_address` is the gRPC data
   plane address. For PG, it's the streaming replication address
   (derived from `OpenContext.data_bind`). The operator stores this in
   `ReplicaInfo` — all operator-side consumers use it only for
   registration/status, not for direct gRPC connections, so the semantic
   change is safe.

3. **No StateReplicatorHandle usage**: The `replicate()` method is not
   called — PG commits go through PG directly. Verified: `replicate()`
   is only called by user code, never by framework code. The
   `ServiceContext` returned from the adapter will have a no-op
   `StateReplicatorHandle` with unused `copy_stream` and
   `replication_stream` (set to `None`).

### No Changes Needed

- `PartitionState` atomics — PgMonitor writes the same fields
- `LifecycleEvent` enum — Open/ChangeRole/Close/Abort are generic
- `StateProviderEvent` — not used for PG (adapter handles PG ops directly)
- Operator workflows use the transport-agnostic `ReplicaHandle` status and
  correlated-action surface; `PartitionDriver` is read-only recovery.

### Future: Trait-based Replicator

If more external-replication databases are added, extract a `Replicator`
trait:

```rust
pub trait Replicator: Send + Sync {
    async fn change_role(&self, epoch: Epoch, role: Role) -> Result<()>;
    async fn update_epoch(&self, epoch: Epoch) -> Result<()>;
    async fn build_replica(&self, replica: ReplicaInfo) -> Result<()>;
    fn state(&self) -> &Arc<PartitionState>;
    fn data_address(&self) -> &str;
    fn abort(&self);
}
```

This is a Phase 2 concern — for now, the PgReplicatorAdapter directly
constructs a `ReplicatorHandle` using the existing struct.

---

## Implementation Plan

### Phase 1: Core Infrastructure

- PgInstanceManager: initdb, start, stop, promote, configure
- PgMonitor: poll LSN, update PartitionState
- PgReplicatorAdapter: satisfy ReplicatorHandle contract
- PgService: lifecycle event handler (Open, ChangeRole, Close)
- Basic client gRPC: Execute SQL, return rows
- Integration test: 3-pod cluster, write on primary, read on primary

### Phase 2: Failover & Replication

- Streaming replication setup (primary_conninfo, standby.signal)
- pg_basebackup for new replica builds (copy protocol)
- Failover test: kill primary, verify promotion + data survival
- Switchover test: graceful primary swap with write fencing verification
- ~~pg_rewind for demoted primary rejoin~~ → Done: ReconfigureStandby
  detects demoted primary, tries pg_rewind first, falls back to
  pg_basebackup

### Phase 3: Robustness

- ~~Epoch fencing: UpdateEpoch triggers pg_rewind~~ → Simplified: no-op.
  Secondaries reconnect via ReconfigureStandby; demoted primaries rejoin
  via pg_rewind (fallback: pg_basebackup) in ReconfigureStandby.
- ~~Synchronous replication~~ → Done in Phase 2 (synchronous_standby_names)
- ~~Quorum health: committed_lsn~~ → Done in Phase 2 (PgMonitor flush_lsn)
- ~~Crash recovery~~ → PG handles natively (tested via failover test)

---

## Design Decisions

### ~~DD-1: UpdateEpoch~~ (Simplified)

Originally planned to detect WAL divergence and trigger pg_rewind in
`UpdateEpoch`. Abandoned because:
1. `ReplicatorControlEvent::UpdateEpoch` carries only `epoch`, not
   `previous_epoch_last_lsn` — no divergence threshold available.
2. Secondaries' `PartitionState.committed_lsn()` is always 0 (only
   primary's PgMonitor sets it) — comparison is meaningless.
3. Zombie primaries are removed by the operator — they never receive
   `UpdateEpoch`.
4. Surviving secondaries are reconnected via `ReconfigureStandby`.

`UpdateEpoch` is a no-op. Old primaries rejoin via `BuildReplica` →
`CloneFrom` (full pg_basebackup).

### DD-2: Replica naming for `synchronous_standby_names`

PG's `synchronous_standby_names` uses `application_name` strings to
identify standbys. kuberic uses `ReplicaId` (i64). We need a mapping.

**Resolution**: Each PG standby connects with
`application_name = 'kuberic_{replica_id}'` in its `primary_conninfo`.
This is set automatically during `pg_basebackup -R` by appending to the
generated `primary_conninfo`, or by writing `postgresql.auto.conf`
after cloning.

```
primary_conninfo = 'host=primary port=5432 application_name=kuberic_2'
```

The `UpdateCatchUpConfiguration` handler maps `ReplicaSetConfig.members`
to PG's format:

```rust
ReplicatorControlEvent::UpdateCatchUpConfiguration { current, reply, .. } => {
    // Build synchronous_standby_names from replica IDs
    let standby_names: Vec<String> = current.members.iter()
        .filter(|r| r.id != self_replica_id && r.role != Role::None)
        .map(|r| format!("kuberic_{}", r.id))
        .collect();

    let quorum = current.write_quorum.saturating_sub(1); // subtract self
    if !standby_names.is_empty() && quorum > 0 {
        let names = standby_names.join(", ");
        let sql = format!(
            "ALTER SYSTEM SET synchronous_standby_names = 'ANY {quorum} ({names})'"
        );
        client.execute(&sql, &[]).await?;
        client.execute("SELECT pg_reload_conf()", &[]).await?;
    }
    // Also store primary address from members for future pg_rewind
    let _ = reply.send(Ok(()));
}
```

**Convention**: `kuberic_{replica_id}` is the application_name for all
PG standbys. This is consistent, collision-free, and maps trivially
between kuberic's numeric IDs and PG's string names.

### DD-3: Epoch vs Timeline — Two-Layer Fencing

kuberic's epoch and PG's timeline are orthogonal fencing mechanisms at
different layers. They naturally align (each failover bumps both) but
neither needs to be injected into the other.

| Layer | Mechanism | What it fences |
|-------|-----------|---------------|
| kuberic (control plane) | Epoch `(dln, cn)` | Operator won't send commands to wrong-epoch replicas |
| PG (data plane) | Timeline ID | PG rejects WAL from a different timeline |
| PG (write plane) | `synchronous_commit` | Zombie primary's commits hang when standbys disconnect |

**Why no injection**: PG has no GUC or extension point for custom
fencing metadata. Timelines are incremented automatically on
`pg_ctl promote` — they track the same events as kuberic epoch bumps
but from the data layer's perspective.

**Observability (Phase 3, optional)**: Store kuberic epoch in a PG
metadata table for DBA debugging. Not required for correctness — the
operator tracks epoch/role/LSN in its own state (PartitionDriver +
PartitionState atomics + CRD status).

```sql
-- Optional: created in Phase 3 for debugging convenience
CREATE TABLE IF NOT EXISTS kuberic_metadata (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
```

## Open Questions

### ~~OQ-1: PG Binary Distribution~~ (Resolved)

The PG binary directory is a required CLI argument (`--pg-bin`).
Users pass their system-installed path, e.g.
`--pg-bin /usr/lib/postgresql/16/bin`. For container images, the path
is baked into the entrypoint.

### ~~OQ-2: Connection String Management~~ (Resolved)

Clients connect directly to PostgreSQL's TCP port. In K8s, the operator
maintains a read-write Service (`-rw`) with label selectors pointing to
the current primary pod. On failover/switchover, the operator relabels
pods and the Service routes automatically. For local testing, clients
connect to `localhost:<port>` — the test harness tracks which instance
is primary.

### OQ-3: WAL byte limits and headroom (deferred)

Physical-slot lifecycle is implemented, but `max_slot_wal_keep_size` and
free-space headroom admission are not configured by Phase 4. WAL bytes for
needed slots remain unbounded. The later capacity design must combine a chosen
limit with explicit invalidated-slot recovery/rebuild; this historical proposal
is not an implemented bounded-disk guarantee. Terminal slots are reclaimed by
the current build-retirement path independently of that future policy.

### ~~OQ-4: Read Replicas~~ (Resolved)

No read replicas. Secondaries exist for failover only — they do not
serve client queries. However, `hot_standby = on` is required so that
PgMonitor can query standby LSN via SQL (`pg_last_wal_receive_lsn()`).
Clients don't connect to standbys — the K8s Service routes only to the
primary.

---

## Known Problems

### KP-1: Split-brain window during network partition

If the old primary is partitioned from the operator during failover,
`ChangeRole(None)` and `UpdateEpoch` never reach it. The old primary's
PG stays running and writable until:
- (a) `synchronous_commit = on` causes commits to hang (no standby to
  ACK — all standbys have moved to new primary's timeline), or
- (b) PG's `wal_sender_timeout` disconnects stale standbys, or
- (c) K8s restarts the pod (if a liveness probe detects isolation)

**Mitigation**: `synchronous_commit = on` is mandatory (Required
Configuration). This provides PG-native write fencing — the old primary
blocks on commits when standbys disconnect. Clients with stale connections
see commit timeouts, not silent data divergence. Accepted residual risk:
async commits during the partition window (if any) may be lost.

### KP-2: Promotion latency with large WAL backlog

`pg_ctl promote` may take minutes if the standby has a large WAL backlog
to replay (e.g., after a crash-recovery scenario with many un-replayed
WAL segments). The 60-second timeout in `ChangeRole(Primary)` may be
insufficient. If promotion times out, the operator invokes switchover
rollback (A3 pattern) and tries another candidate.

### ~~KP-3: Demoted primary lacks `standby.signal` — dual-primary risk~~ (Fixed)

**Severity**: ~~must-fix~~ → fixed

After switchover, the old primary is demoted to `ActiveSecondary`.
`ReconfigureStandby` rewrites `primary_conninfo` and restarts PG, but
previously never created `standby.signal`. PG 12+ requires this file to
start in standby mode — without it, the instance restarts as a
read-write primary, creating a dual-primary split-brain.

**Fix applied**: `ReconfigureStandby` now detects when the target was a
primary (`standby.signal` didn't exist) and handles timeline divergence:
1. Stop PG
2. Try `pg_rewind` from new primary (fast — copies only diverged pages)
3. If `pg_rewind` fails (WAL recycled, corruption): full `pg_basebackup`
4. Create `standby.signal`, patch config, start as standby

This matches CNPG's approach. In production with `wal_keep_size`
configured, `pg_rewind` typically succeeds. In tests with aggressive WAL
recycling, the fallback to `pg_basebackup` handles it correctly.

### KP-4: PG-level write fencing not implemented

**Severity**: ~~must-fix~~ should-fix (UX improvement, not correctness)

The design specifies `ALTER SYSTEM SET default_transaction_read_only = on`
+ `pg_reload_conf()` during demotion (§Client Access & Write Fencing),
but no implementation exists. The framework's `revoke_write_status()` only
sets an in-memory atomic that PG clients never check.

**Why not must-fix**: `synchronous_commit = on` (mandatory config) already
provides correctness fencing — writes on the old primary hang because no
standby ACKs them (standbys have disconnected or moved to the new primary's
timeline). The `ReconfigureStandby` restart follows within seconds as a
hard fence. Data never actually diverges; the worst case is a client
seeing a commit timeout instead of an immediate read-only error.

**Design** (see §Client Access & Write Fencing for full details):

The adapter's `ChangeRole(ActiveSecondary)` handler is the implementation
hook — it fires immediately after `revoke_write_status()` during
switchover. When demoting from Primary:
1. Connect to PG
2. `ALTER SYSTEM SET default_transaction_read_only = on`
3. `SELECT pg_reload_conf()`

This provides a soft fence (new transactions default to read-only).
The hard fence follows seconds later when `ReconfigureStandby` restarts
PG as a physical standby (KP-3 fix).

On promotion (`ChangeRole(Primary)`), clear the fence:
`ALTER SYSTEM SET default_transaction_read_only = off` + reload.

**Limitation**: `default_transaction_read_only` is bypassable by clients
(`SET SESSION` / `BEGIN READ WRITE`). Acceptable for kuberic's switchover
window (~2 seconds), combined with `synchronous_commit = on` which causes
writes to hang when standbys disconnect.

### KP-5: `start()` TOCTOU race — double PG launch possible

**Severity**: ~~must-fix~~ should-fix (defensive hardening — not reachable in practice)

`PgInstanceManager::start()` drops the `child` mutex guard after checking
`is_some()`, spawns PG without the lock, then reacquires to store the
child. Two concurrent callers can both pass the check and spawn two PG
processes on the same data directory and port.

**Practical risk**: Low. All callers (`CloneFrom`, `ReconfigureStandby`)
call `stop()` first, so `is_some()` always returns false. The driver
serializes all operations per-replica — concurrent RPCs to the same
pod's PgDataService don't happen under normal operation. This is part
of the broader KP-8 (unserialized concurrent mutation).

**Fix**: Hold the `MutexGuard` across the entire `start()` body including
spawn. `tokio::sync::Mutex` is designed to be held across `.await`.

### ~~KP-6: `synchronous_standby_names` hardcodes `ANY 1`~~ (Fixed)

**Severity**: ~~should-fix~~ → fixed

`configure_sync_standbys` always set `ANY 1` regardless of
`ReplicaSetConfig.write_quorum`. For a 5-replica set with `write_quorum=3`,
PG should require `ANY 2` standby ACKs but only required 1.

**Fix applied**: Derives quorum from `write_quorum`: `quorum =
write_quorum.saturating_sub(1).max(1)`, then uses `ANY {quorum}`.
Also added SAFETY comment for the `format!`-based SQL (SF-7).

### KP-7: PgMonitor missing `fault_tx` — SQL failures unreported

**Severity**: should-fix (low urgency — narrow edge case)

Design specifies PgMonitor should have `fault_tx` and report
`FaultType::Permanent` after 3 consecutive SQL failures. Implementation
has no `fault_tx` field — SQL failures are silently dropped. Two
overlapping health monitors exist: `PgInstanceManager` checks
`pg_isready` (process-level, reports faults), `PgMonitor` checks SQL
(application-level, silent).

**Gap**: PG running but unable to execute SQL (corrupt catalog,
max_connections) goes unreported. However, `pg_isready` catches most
real failures (process death, socket unavailable). The gap is narrow:
PG alive but SQL-broken is unusual — corrupt catalogs typically crash PG
(caught by health monitor), and max_connections is unlikely in an
example app. Stale `current_progress` would be noticed by the operator
when values stop advancing.

### KP-8: Concurrent PgDataService/adapter mutation unserialized

**Severity**: ~~should-fix~~ consider (theoretical — not reachable in practice)

`PgDataServiceImpl` and `PgReplicatorAdapter` both hold
`Arc<PgInstanceManager>`. Multi-step operations (`CloneFrom`:
stop→clear→backup→start; `ReconfigureStandby`: rewrite→stop→start) are
not serialized against adapter control events (`ChangeRole`, `promote`).
The `Mutex<Option<Child>>` serializes field access but not operational
sequences.

**Why low risk**: The RPCs are **cross-pod** — the primary's adapter
calls `CloneFrom`/`ReconfigureStandby` on the **secondary's**
PgDataService. The driver blocks on each operation (awaits reply before
sending the next event). So the secondary's adapter never receives a
control event while its PgDataService is mid-operation. Concurrent
mutation requires an external actor calling RPCs directly, which doesn't
happen under normal kuberic operation.

**Fix (if needed)**: Route all PG mutations through the adapter's control
channel, or add a top-level operation lock.

### ~~KP-9: Zombie processes after PG crash~~ (Fixed)

**Severity**: ~~should-fix~~ → fixed

Design specified `child.wait()` for immediate crash detection.
Implementation used `pg_isready` polling — 6+ second detection delay.
After crash: (1) child process was a zombie (never reaped), (2) `child`
field stayed `Some` so `start()` thought PG was running.

**Fix applied**: Added a **process exit monitor** task alongside the
existing `pg_isready` health monitor. The exit monitor polls
`child.try_wait()` every 500ms — on unexpected exit (shutdown token
not cancelled), it immediately reaps the child, clears the `child`
field to `None`, and reports `FaultType::Permanent`. The `pg_isready`
health monitor is retained as a complement (catches PG hung but process
alive). Both monitors now check `shutdown.is_cancelled()` before
reporting fault (also fixes KP-11).

### KP-10: `rewrite_primary_conninfo` discards connection parameters

**Severity**: should-fix

Rebuilds `primary_conninfo` from scratch: `host={h} port={p}
application_name={name}`. All other params from `pg_basebackup -R`
(user, sslmode, channel_binding, etc.) are silently dropped. Breaks in
any non-`trust` auth environment.

**Fix**: Parse existing conninfo key=value pairs, replace only host/port/
application_name, preserve the rest.

### ~~KP-11: Health monitor false fault during intentional stop~~ (Fixed)

**Severity**: ~~should-fix~~ → fixed (addressed as part of KP-9 fix)

Race: monitor's `pg_isready` runs between shutdown token cancellation and
pg_ctl stop completion. If already at 2 accumulated failures, the
stop-induced failure triggered spurious `FaultType::Permanent`.

**Fix applied**: Both monitors (exit monitor and pg_isready monitor) now
check `shutdown.is_cancelled()` before sending `FaultType::Permanent`.
