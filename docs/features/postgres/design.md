# PostgreSQL: Native Replication on Kuberic Public V1

**CURRENT implementation.** The
[ordinary-engine host target](../kuberic/engine-neutral-hosting.md#proposed--ordinary-engine-boundary)
and [R3 PostgreSQL migration](../kuberic/sf-v1-semantic-alignment-plan.md#hosting-redesign-roadmap--not-implemented)
are **PROPOSED / NOT IMPLEMENTED**. This design/docs delivery changes no
PostgreSQL runtime, on-disk format or qualification result.

The `postgres-replicated` package supplies an independent native engine for the
level-triggered Public V1 runtime and its supported custom-controller route.
Bounded local qualification includes actual controller-driven formation and
native database evidence. Images, deployment assets and distribution
remain deferred; there is no PostgreSQL KinD or live-cluster coverage.
This is experimental software, not a production deployment guide.

The [current engine-neutral host](../kuberic/engine-neutral-hosting.md#current--delivered-bounded-hosting) uses one retained
authority/lifetime coordinator for this independent allocation and built-in V1.
Host-installed configuration, exact host remote Build state and optional native
idle observations are separate facts. PostgreSQL's `built_idle` query still
means completed native copy, unlike V1 membership that can appear during copy;
neither grants voting or SQL access. Production callback repeatability remains
conservative. Actual controller-dispatched hosted terminal cases retain child
Close failure after successful cleanup and reject lost native joins, alongside
the real backup/replay/SQL-fencing formation witnesses.

Use the current protocol/schema declared by the
[runtime guide](../kuberic/classic-v1-runtime.md#production-policies-and-security).
Prior unsupported agent/provider formats reject unchanged; there is no import,
mixed-version or rolling mode implied by custom-controller integration.

The current implementation uses a separate checksummed PostgreSQL metadata
store. It remains retained in the hosting redesign. Removal is separately
deferred behind the [complete metadata-disposition gate](stateless-metadata-prerequisites.md),
not an assumption that three generic changes make it safe.
The earlier [restart-stateless proposal](stateless-metadata.md) is superseded
historical research, not an active implementation prescription.

## Architecture and Authority

`PgService` implements `StatefulServiceReplica`; its `open` returns the
service-created `PgReplicator` through the existing `ReplicatorFactory` and
`ReplicatorInterfaces`. PostgreSQL implements ordinary `Replicator` and
`PrimaryReplicator`. It supplies neither an operation/copy `StateReplicator` nor
a dummy `StateProvider`. There is no alternate native/external public mode or
driver hierarchy.

| Owner | Responsibility |
|---|---|
| Kuberic protocol/controller | Generic SF-inspired configuration, role, failover, handoff and membership choreography |
| Private agent lifecycle host | Durable identity, authority, effects, selected builds, exact process sessions, proof-before-publication access, callback fencing, Close/Abort and restart reconstruction |
| PostgreSQL custom replicator | WAL, system identity, timeline/LSN evidence, physical backup/rewind/recovery, synchronous policy, receiver drainage, replay and promotion |

The agent delivers exact incarnation, session, endpoint, role and frozen progress
descriptions through `ReplicaInformation` / `ReplicaSetConfiguration` and the
existing configuration/build/catch-up callbacks. Authority stores and mutation
capabilities remain private. The production factory attaches its own
`CustomReplicatorControl` to the same creation; it does not borrow a built-in
bundle. The separate
[companion contract](../kuberic/replicator-boundary.md#custom-controller-integration)
adds exact inbound copy authority, coherent idle observations and owned drainage
without adding methods to the protected native traits. Although attachment is
optional for a bundle, current controller Build requires it. This is a real
extra boundary beyond SF's ordinary custom-replicator integration, not justified
merely by describing it as local.

PostgreSQL uses the same common lifecycle owner as the built-in engine but does
not register the built-in replication/copy data plane. Delayed native callback
completion is accepted only under the original authority, process sessions and
lifecycle generation. Authority/session replacement, Close and Abort cancel or
stale the result before generic progress, access, build or removal credit is
published.

`GetCurrentProgress` exposes durable WAL end for ordering, not proof of replay;
catch-up capability reports the beginning of retained WAL. A raw peer scalar,
process readiness or successful backup cannot mint quorum/build credit.
PostgreSQL validates its own lineage and completion evidence before callbacks
complete. Generic reports and controller status contain no PostgreSQL recovery
certificates. Public V1 advertises the custom replicator's actual endpoint;
database evidence travels only over application-owned `pgdata.v2`.

## SQL Access and Acknowledged Writes

The retained partition handle's independent read/write statuses govern access,
not role notifications, progress polling or PostgreSQL promotion alone. An
owned periodic worker reconciles actual access. The selected host denies
ordinary stable-secondary reads and writes despite native replay; internal
administrative evidence is not a client read grant. Idle/unadmitted replicas
also remain closed.

Ordinary clients use the non-superuser database-owner role `kuberic_app`, database
`kuberic`, and `synchronous_commit=remote_apply`. PostgreSQL owns synchronous
standby selection using exact session-qualified application names. Before a
policy change, it durably invalidates recovery metadata, applies and reads back
the native policy, then distributes the matched policy to the exact eligible
standbys. SQL cannot reopen on stale policy metadata after process restart.
Deliberately selecting local/asynchronous acknowledgement is outside the
acknowledged-data guarantee.

Host authority/access invalidation precedes application-owned native drainage;
it does not instantly mediate SQL statements. The selected Public V1 denial and
terminal paths stop and reap the owned PostgreSQL run, join native gateways and
leave PostgreSQL stopped. Separate authorized role/configuration work may later
start it; closure does not unconditionally restart it or acknowledge a native
fence merely from host status
([drain:1084–1156](../../../examples/postgres/src/public_v1.rs#L1084-L1156),
[denial:1185–1201](../../../examples/postgres/src/public_v1.rs#L1185-L1201)).

HBA reload and `pg_terminate_backend` alone miss accepted, unauthenticated
sockets. Completed process fencing disconnects ordinary, retained administrative
and pre-authentication connections; later regrant cannot revive them. The
separate direct/legacy `PgAccessController::close_external` can restart with
closed HBA rules; that is not the selected controller route or the
actual-controller witness's closure postcondition.

A transaction already inside PostgreSQL and waiting for synchronous
acknowledgement when connectivity is lost has an **unknown outcome**, not a
promised rollback. A unique write attempted after the completed fence is
definitively rejected without a database side effect.

### Administrative Trust and Supervisor Loss

Database and OS administrators are trusted. This example uses local trust
authentication, loopback application access, local administrative sockets and
trust-authenticated physical replication (including an IPv4 replication HBA
rule). PostgreSQL listens on all addresses; this is not hardened network
isolation. Do not expose the database/replication port to untrusted networks.
Agent control uses the configured bearer credential over HTTP. The selected
native Public V1 route additionally validates exact host/engine authority; this
is not complete coordination-authentication or TLS hardening. Managed settings
must not be replaced or bypassed.

Completed fences disconnect retained administrative test sessions, but do not
protect against hostile administrators changing configuration or relaunching
PostgreSQL. API unreachability is not physical termination evidence. Abrupt
supervisor loss, including a host killed while PostgreSQL independently survives,
is outside the containment guarantee. The supervisor and database must share a
managed lifetime; there is no independent orphan self-fencing mechanism.

## Native Build and Catch-Up

The controller installs exact inbound authority before source dispatch.
PostgreSQL's hosted native Build envelope carries that full authority: both
replica/storage/process endpoints, history, epoch, configuration and operation/
Build attempt. The creation-bound source context resolves only its accepted
running Build. Target installation alone is not source liveness; native source
inspection independently validates the source journal and grant.

The selected native owner revalidates around gateway creation, destructive
backup/receiver work, asynchronous completion cuts and publication. Native
`pg_basebackup`, compatible system/timeline history, receive **and replay** at
the captured source boundary, and a checkpoint establish its actual completion.
Its exact process-local completion cache also revalidates durable/physical
history and live source/target authority; it is not a durable generic-agent
database receipt or permission to reopen revoked ingress.

Target preflight and cached source-inspection reads observe captured work
cancellation without acquiring process cleanup authority. Graceful Close can
interrupt those reads before its final durable observation and lifetime join.
Source slot retirement revalidates the hosted attempt before each new mutating
SQL retry; this does not roll back an already-issued SQL statement.

PGDATA helpers, native gateways and their retained join owners remain
application-owned. A spawned gateway enters creation-owned grants before any
post-start validation await; cancellation cannot detach a provisional listener
from terminal joining. Inbound task-join and gateway-drain failures remain
errors across later terminal observers, not handles polled after completion.
Terminal and periodic paths transfer every gateway to retained drain ownership
before awaiting any exit. Each completed drain outcome is shared immediately,
so dropping a Close while another exit is pending loses neither the remaining
gateways nor an already-observed failure.
Revocation removes copy ingress, not copied data or voter
membership. A different uncertain receiver attempt requires containment/process
replacement, not an in-place metadata reset. Failed/interrupted custom callbacks
can remain `ContainmentPending` until explicit terminal cleanup; this bounded
route does not promise arbitrary failed-Build automatic reformation.

Successful built-idle ownership and last installed native membership are
separate from live transport/readiness. Same-history epoch advancement can
drain transport while retaining the exact predecessor for joint catch-up;
completed copy authority is not reused as a current grant. Secondary host
configuration authority precedes primary native Peer admission and supplies
neither a primary-only callback nor serving access.

An empty uninitialized native history reports the unknown sentinel, not a
fabricated zero. Initial singleton authority requires exact successful New-Open
provenance and both explicit sentinel query results; this local initialization
exception changes neither retained election nor native progress semantics.
`WriteQuorum` with marked successors remains explicitly unsupported; ordinary
unmarked waits and `All` retain their qualified native behavior.

Selected implementation: [factory:294–311](../../../examples/postgres/src/service.rs#L294-L311),
[companion:547–641](../../../examples/postgres/src/public_v1.rs#L547-L641),
[retained drainage:701–800](../../../examples/postgres/src/public_v1.rs#L701-L800),
[Close joins:1273–1323](../../../examples/postgres/src/public_v1.rs#L1273-L1323),
[Build/replay:2365–2698](../../../examples/postgres/src/public_v1.rs#L2365-L2698)
and [native/controller qualification:185–1412](../../../examples/postgres/src/public_controller_tests.rs#L185-L1412).

Physical replication slots belong to exact incarnations, not quorum votes.
Slots reserve WAL before build and survive internal reconnection/restart.
Session-qualified names separately determine acknowledgement eligibility.
Retired consumers drain before slot deletion; an in-use slot is not reported
removed. Unix sockets live in `PGDATA/pg_stat_tmp`, excluded by backup/rewind.

## Failover and Planned Switchover

The application-owned recovery protocol below has separate native/direct and
manual-host evidence. It is not a claim that the bounded real-controller
formation witness executes arbitrary restart/failover/reformation scenarios.

Generic hosting installs write-closed authority and awaits the existing primary
role callback. PostgreSQL, not a shared native selector, performs recovery:

1. Collect fresh exact-session observations for the last accepted synchronous
   policy. Require **`R + W > N`**: observed responders plus acknowledgement
   count must intersect every eligible synchronous standby acknowledgement set.
   Missing, restarted or incompatible optional peers contribute nothing to `R`
   but remain in persisted `N`; neither `N` nor `W` is reduced. Local authority
   and policy must still be exact.
2. Journal that initial set. Responders persist revocation, remove old-primary
   connection settings, verify WAL receiver exit, retain received/replay progress
   and restart closed.
3. Re-observe the same sessions/policy and compatible lineage after receiver
   drainage; revalidate the intersection before final candidate ordering by
   received WAL, then replay WAL, then exact identity.
4. The selected candidate replays the selected boundary, promotes, checkpoints
   and verifies lineage before completing `ChangeRole(Primary)`. An unsafe or
   non-selected candidate refuses activation.

Draining the intersecting set prevents further supported synchronous
acknowledgements from the old source; it is not an attestation that the old
postmaster died. Surviving standbys must reconnect to the exact promoted primary
and timeline before regaining synchronous eligibility. Former primaries remain
stopped until an authorized rewind or fresh build.

Planned switchover uses the existing two SF `All` catch-up calls around access
revocation. Source demotion checkpoints and persists a handoff intent, completes
verified shutdown, and distributes a durable stopped receipt before returning.
The target revalidates that receipt and replay boundary before promotion.
Generic hosting journals only successful callback/effect receipts; access grant
is separate. Restart can resume the same evidence or stay closed, never infer
success from a role flag.

## Scaling, Replacement and Restart

Sequential scale-up builds/adopts one exact fresh identity at a time. PC/CC
configuration precedes current-only admission, and candidates stay fenced until
their selected build completes. Secondary removal closes writes, freezes a
boundary under SF `All`, checks exact-session witnesses, commits reduced authority
and persists terminal retirement only after application Close. Witness scalars
do not advance custom quorum progress.

Replacement uses a fresh incarnation and storage root for the same ordinal;
delayed old-session work is rejected. A former primary can only rejoin through
a selected rebuild. Direct primary removal is not implemented: use planned
switchover followed by secondary removal.

Agent SQLite metadata, checksummed PostgreSQL metadata and PGDATA have distinct
owners. Schema 5 binds canonical application/PGDATA paths before authorized
creation. First-open permission survives interruption but is consumed before
readiness. Valid completed `initdb` is reused and managed settings repaired;
partial/invalid established storage is never silently erased or reinitialized.
Missing files, changed paths/symlink targets, corrupt identity/lineage or
ambiguous evidence fail closed with explicit faults.
Unexpected durable-role/recovery-signal mismatches or incomplete recovery without
an authorized native build stop and reap the captured PostgreSQL generation before
publishing a permanent fault. A private reader/writer gate serializes progress/status
validation with native role changes and durable role publication, without blocking
access regrant behind delivery of an old reader's helper error. Before
native promotion, the private election journal records the exact candidate,
process session and process generation after final quorum/replay verification.
An interrupted role-publication gap is recognized only with that intent, unchanged
authority and accepted policy, exact final responders/boundary, closed access and
compatible promoted lineage. Same-generation retries complete the existing election;
cold storage inspection does not authorize a retired session to activate. Build and
demotion transitions retain their existing validation. A delayed mismatch from a
retired generation cannot fence its successor, and unrelated recovery metadata
never exempts a true role/signal mismatch.

Application-only and whole-agent-host restart reconstruct durable state with
fresh sessions. Accepted standbys wait for validated current source connections.
On whole-agent restart a persisted custom-replicator grant remains desired, not
immediately effective: transient access reconciliation keeps the control service
live and reports access pending while application SQL stays closed. Progress/status
reconciliation retries that grant only under the restored authority until exact
peer discovery, native policy readback and acknowledgement availability converge.
New access/authority fencing supersedes the deferred grant; other startup errors
remain fatal. The default replication engine's restart behavior is unchanged.
Lost quorum closes writes without discarding acknowledged data; accepted
secondary reads remain possible. Restoration requires fresh evidence and native
policy readback, not old scalar journals.

## Process Ownership and Failure Semantics

Linux pidfds, child-subreaper support and readable `/proc` are prerequisites.
The package binary re-executes a dedicated single-threaded subreaper for each
PostgreSQL/helper launch; library hosts need `postgres-replicated` beside their
executable. No separate installed helper binary is required.

Readiness verifies the actual postmaster's ownership. Shutdown signals retained
pidfds, not mutable PID-file targets, and uses bounded fast shutdown/escalation.
Success requires reaping every owned identity, not merely observing exit.
Helpers (`initdb`, control-data/readiness probes, promotion, backup and rewind)
share cancellation-safe ownership. Abort, signals, dropped futures and partial
startup join cleanup; unrelated processes are never targets.

Durable process-generation high-water marks prevent reuse after reopen; delayed
cleanup/results/faults cannot affect a successor generation. Exhaustion is
terminal, not wrapping. Fatal ownership/cleanup failures remain retained.
Transient helper deadlines stop the exact run closed and permit a safe retry.
Fault reporting awaits bounded durable agent acknowledgement; acknowledgement
or cleanup failure produces a nonzero executable exit.

Application metadata commits serialize cancellation with rename/fsync and
publication. Once commit wins, cancellation joins it. Active/suspended builds
are bounded to 16, retained IDs to 64 per admitted epoch, IDs to 512 bytes and
metadata to 4 MiB. New epochs compact old IDs into a rejection watermark, never
TTL-based forgetting. Exhausted budgets fail closed.

## Local Usage and Configuration

Run as an unprivileged Linux account with compatible local PostgreSQL server and
client binaries (`initdb`, `postgres`, `pg_ctl`, `pg_basebackup`, `pg_rewind`,
`pg_controldata`, `pg_isready`). PostgreSQL 16 is the validated host major;
there is no multi-major compatibility claim.

```sh
cargo run -p postgres-replicated -- --help
cargo run -p postgres-replicated -- \
  --resource-uid example --replica-id 1 --pod-uid replica-1 --pvc-uid storage-1 \
  --data-root ./postgres-state --pg-bin /usr/lib/postgresql/16/bin \
  --bearer-token "$KUBERIC_AGENT_BEARER_TOKEN"
```

This starts an authority-waiting host, **not** a writable standalone database.
The real controller must initialize exact history and drive Open, role, epoch,
configuration and access admission. Direct fixture choreography is not a
substitute for controller qualification. No PostgreSQL deployment/bootstrap CLI
or container distribution is supplied.

| Options / environment | Meaning |
|---|---|
| `--resource-uid`, `--replica-id`, `--pod-uid`, `--pvc-uid` / corresponding `KUBERIC_*` | Exact resource/storage incarnation; replica ID must be positive |
| `--data-root` / `KUBERIC_DATA_ROOT` | Agent root (`.kuberic/agent.sqlite3`); default application and PGDATA roots beneath it |
| `--application-root` / `KUBERIC_APPLICATION_ROOT`, `--pg-data` / `PGDATA` | Optional separately bound application metadata and PostgreSQL directories |
| `--pg-bin` / `KUBERIC_PG_BIN`, `--pg-port` / `PGPORT` | Required binary directory and SQL port (default 5432) |
| `--control-address`, `--replication-address`, `--application-address` | Agent control/operation transport and PostgreSQL coordination listeners (loopback 50051/50052/50053 by default); corresponding `KUBERIC_*_ADDRESS` variables |
| `--control-endpoint`, `--replication-endpoint`, `--application-endpoint` | Advertised endpoints; corresponding `KUBERIC_*_ENDPOINT` variables |
| `--bearer-token` / `KUBERIC_AGENT_BEARER_TOKEN` | Shared agent and application-coordination credential |
| `--peer-routes` / `KUBERIC_PEER_ROUTES` | Optional JSON file of exact `identity`, `control`, `replication` routes, at most 32 entries / 64 KiB |

Route identities contain `replicaId`, `instanceId`, `agentGeneration`; there is
no ordinal-only fallback. PostgreSQL coordination uses the endpoint returned by
the custom replicator, not the agent's operation-stream listener.
The application repairs managed port/socket, `wal_log_hints`, `hot_standby`,
`wal_level=replica`, synchronous durability and closed access settings on startup.
Configuration is implementation-owned, not a general PostgreSQL tuning API.

## Local Validation

```sh
just nextest-postgres-smoke-inventory
just nextest-postgres-smoke
```

The smoke inventory validates the live catalog before PostgreSQL starts.
The recipe builds the owned command binary, then runs its selected native and
actual-controller cases serially in one package-only pass. Selectors and
evidence limits are in the [testing guide](../kuberic/testing.md#custom-native-controller-reproduction-and-limits);
do not copy test totals or historical removed selectors.
Public V1 fixtures use
`KUBERIC_POSTGRES_PUBLIC_V1_TMPDIR` when set; the smoke recipe otherwise uses
`/dev/shm` when at least 2 GiB is available and falls back to `/tmp`. The
full target is reserved for the dedicated
resource-owning validation path or explicit requests.

The actual controller fixture constructs inert production PgService instances
and owns their native coordination listeners. It checks pre-Role sentinel/Open
provenance, committed seed data, real source/target Build, built-idle state,
exact joint/current-only admission, live native replay, source/target
invalidation, synchronous SQL/replay and completed native fencing. Owned short
socket-path aliases identify the same declared backing storage, not a different
storage incarnation. Administrative standby queries prove replay only, never
ordinary secondary-read authorization.

Hosted native retries share one retained exact attempt, while waiting/cache
observers remain outside mutation cleanup. Captured cancellation and coherent
host checks are repeated after serialized acquisition and preparation awaits.
Graceful Close retains its bounded final native observation before cancelling
the process lifetime; an already aborted owner does not invent that observation.

PostgreSQL deliberately does not claim KSF-02's complete
false/true/error/ambiguity matrix, KSF-10's Replicator-owned endpoint lifetime,
KSF-11's application-owned service-address publication, or KSF-15's full
callback replay matrix. Those rows remain executable in the strict custom
fixture; the PostgreSQL scenarios assert their non-applicability rather than
treating scenario execution as proof.

Tests discover common PostgreSQL installation directories (16, 17, 15 in that
order on Debian-style hosts), then `pg_config --bindir`. `--pg-bin` configures the
executable, not a test-runner override. Missing binaries fail the prerequisite;
tests do not silently skip. Fixtures own their worker stacks; no extra runner
stack override is required.

Direct/native and manual-host fixtures remain separate from the real-controller
cases in the live catalog. Their recovery/slot/access observations do not
automatically qualify controller recovery or a different database engine.
Teardown checks owned processes, helpers, gateways and listeners; failed joins
propagate instead of becoming successful containment.

CI builds one all-features nextest archive, then runs four deterministic hash
partitions on isolated runners. The `postgres` profile and test group both
enforce one test process at a time within each runner; the measured unsafe
same-runner four-way mode is not used. Discover the current inventory and
partition accounting from the validator. PostgreSQL is absent from KinD/live
jobs and selectors.
Shared regressions, partition validation and guard commands are in the
[testing guide](../kuberic/testing.md#postgresql).

## Limitations and Future Work

- Hosting migration R3 removes the companion and controller-attempt grant
  coupling only after R1 production routing and R2 reference-contract coverage.
  PG then owns authenticated engine Active/Retired transfer sessions, target
  quarantine and ordinary Close-after-Abort joins; backup/WAL/lineage/replay,
  SQL fences, slot/process ownership and metadata remain PG responsibilities.
  Production controller recovery/upgrade qualification follows in R4/R5, not
  from current formation tests. See the [single roadmap](../kuberic/sf-v1-semantic-alignment-plan.md#hosting-redesign-roadmap--not-implemented).
- Images, distribution and deployment assets remain future work. No PostgreSQL
  Kubernetes/container dependency, manifest or live test is part of this port.
- Physical-slot lifecycle is implemented, but WAL-retention limits
  (`max_slot_wal_keep_size`) and free-space headroom admission are not. Unavailable
  configured replicas or retryable selected builds can retain unbounded WAL.
  Capacity policy must include invalidated-slot recovery/rebuild, not just a cap.
- Network authentication/TLS hardening, independent orphan containment,
  automatic rolling upgrades and direct-primary-removal orchestration are not
  supplied. Administrative trust and supervisor-loss boundaries above apply.
- Tests prove bounded local traces, not an availability SLO, maximum replica
  count, throughput guarantee or complete failure-interleaving model. Fencing,
  catch-up, restart and handoff may interrupt service without a duration promise.

See the [v1 removal record](../../proposal/v1-retirement-plan.md#workstream-4-postgresql),
[SF callback reference](../../background/service-fabric/references.md) and
[runtime interface boundary](../../../kuberic-runtime/README.md).
