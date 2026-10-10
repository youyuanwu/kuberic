# PostgreSQL Stateless Metadata: Service Fabric Prerequisites

## Status

Required prerequisite architecture. PostgreSQL restart-stateless metadata work
is blocked until every prerequisite in this document is implemented and
verified.

These changes belong to Kuberic's generic Service Fabric-shaped
reconfiguration behavior. They must not introduce PostgreSQL-specific runtime
branches, controller fields, CRD status, agent-store records, protocol
payloads, or callback types.

Passing these prerequisites permits a separate PostgreSQL metadata-disposition
design to begin. It does not by itself prove that the custom replicator can
remove `state-v2.json`. Before removal, every current durable field and nested
build/recovery record must have a reviewed owner and crash-reconstruction rule.

## Decision

Kuberic will follow the Service Fabric V1 ownership model:

- the generic controller and replica agent own epoch, current and previous
  configurations, role and access intent, build authorization, generic
  reconfiguration phase, and generic operation completion;
- the runtime host translates that authority into ordered
  `StatefulServiceReplica`, `Replicator`, and `PrimaryReplicator` calls;
- the PostgreSQL custom replicator owns PostgreSQL semantics and completes a
  callback only after the corresponding native postcondition is true;
- PGDATA owns PostgreSQL identity, timeline and history, recovery role, WAL,
  replication slots, generated PostgreSQL configuration, native signal files,
  and durable database contents;
- process-local observations, queues, peer connections, helper ownership, and
  callback context are disposable.

The design deliberately does not retain exact PostgreSQL witness
certificates, native policy-transition journals, promotion receipts, recovery
elections, build cursors, or application process clocks. Safety is instead
derived from generic epoch and PC/CC authority, ordered callback completion,
fresh progress observations, quorum intersection, PostgreSQL lineage, and
fail-closed access.

This is restart-stateless **application metadata**, not a stateless system.
PGDATA and the generic replica-agent authority remain durable.

## Existing Public Contract

The required public operations already exist:

| Contract | Required meaning |
|---|---|
| `Replicator::change_role` | Complete only after the requested native role postcondition is established; required fencing is part of that postcondition, not an alternative to it |
| `Replicator::update_epoch` | Fence predecessor-epoch work before accepting the new epoch |
| `Replicator::current_progress` | Return progress reconstructed from the durable application copy |
| `Replicator::catch_up_capability` | Return the earliest retained point from which the copy can catch up |
| `PrimaryReplicator::update_catch_up_replica_set_configuration` | Install the supplied current/previous membership and dual-quorum obligation |
| `PrimaryReplicator::wait_for_catch_up_quorum` | On every invocation, establish a fresh catch-up floor at least as high as invocation-time durable local progress and complete only after the requested quorum reaches that floor |
| `PrimaryReplicator::update_current_replica_set_configuration` | Retire the previous configuration only after the handover requirement is satisfied |
| `PrimaryReplicator::build_replica` | Complete only after the authorized copy and catch-up boundary is installed |
| `PrimaryReplicator::on_data_loss` | Reconcile possible quorum loss from application storage and return whether application state changed |
| Partition read/write status | Keep external access closed until generic reconfiguration and native callback postconditions complete |

The public `Replicator`, `PrimaryReplicator`, and `StateProvider` interfaces
must not change. Generic implementation changes may refine when existing calls
are issued, retried, and considered complete.

## Required Generic Changes

### 1. Defer cold role restoration until peers can exist

#### Current conflict

Cold startup restores admitted authority and invokes role callbacks while the
custom-host restoration path is serialized. Fresh peer-session registration
uses the same serialization domain, while the PostgreSQL listener and
application coordination endpoint are created only after host startup
completes.

A truthful Primary or ActiveSecondary callback may need fresh peer sessions to
apply native synchronous membership, establish a source, or validate catch-up.
It cannot complete while peer registration is blocked. Returning
`ReconfigurationPending` currently fails startup instead of leaving the
replica closed and retrying after peers become available.

Relevant integration points include:

- `kuberic-runtime/src/host/hosting.rs` startup restoration;
- `kuberic-runtime/src/host/custom.rs` custom-authority restoration and peer
  registration;
- `examples/postgres/src/main.rs` listener and service startup.

#### Required behavior

Generic startup must have three distinguishable stages:

1. **Closed restoration**
   - restore generic identity, epoch, configuration, role intent, and access
     intent;
   - construct the application and replicator;
   - keep external read and write access denied;
   - install only postconditions that do not require live peers.
2. **Connectivity availability**
   - the generic `ReplicaHost` startup coordinator returns a closed host handle
     after closed restoration rather than waiting for peer-dependent role
     completion;
   - start the application-owned listener and coordination endpoint;
   - use a private, application-neutral host-ready signal to declare that
     application connectivity can now be attempted;
   - permit exact current-session peer registration;
   - reject predecessor sessions and stale registrations.
3. **Deferred role reconciliation**
   - `RecoveryOwner` owns retries after the host-ready signal, peer-session
     registration, or its bounded recovery cadence;
   - retry role/configuration/catch-up work that returned
     `ReconfigurationPending`;
   - preserve the original admitted authority and operation identity;
   - publish access only after the retried callbacks complete.

`ReconfigurationPending` must mean "the admitted operation remains pending and
access remains closed," not "host startup permanently fails."

The host-ready signal is neither role evidence nor an access grant. It only
releases deferred generic reconciliation after the process has made its
application-owned connectivity surface available. The signal and retry path
are private hosting mechanics; they do not change `StatefulServiceReplica`,
`Replicator`, `PrimaryReplicator`, or `StateProvider`.

#### Required invariants

- No peer-dependent callback is required to complete while peer registration
  is blocked by the same owner or lock.
- A new listener or peer session cannot grant role or access by itself.
- Host readiness is monotonic for one process session and is discarded on
  close, abort, or session replacement.
- Retry does not mint a new epoch, configuration, operation, or build
  authorization.
- Close, abort, session replacement, and authority replacement cancel or stale
  deferred work.
- Reports may observe pending restoration but cannot advance it.

### 2. Use monotonic progress for handoff convergence

#### Current conflict

Planned switchover convergence requires an exact equality between the target's
reported catch-up boundary and the source's stored handoff LSN. The catch-up
callback receives a quorum mode rather than that historical scalar, and native
PostgreSQL progress can legitimately advance beyond the handoff point before
the controller observes completion.

After process restart, PostgreSQL can reconstruct its current durable progress
but cannot truthfully recreate or under-report an earlier process-local
boundary. Requiring exact equality therefore creates a need for forbidden
application metadata.

Relevant integration points include:

- `kuberic-controller/src/evaluator.rs` switchover convergence;
- `kuberic-runtime/src/host/custom.rs` catch-up completion and progress
  observation;
- `PrimaryReplicator::wait_for_catch_up_quorum`.

#### Required behavior

The generic handoff LSN remains a durable generic reconfiguration boundary.
Every `wait_for_catch_up_quorum` invocation must first establish a fresh
generic catch-up floor:

1. after entering the callback under the stable admitted configuration and
   process session, the replicator samples its own durable local progress;
2. the invocation floor is the greater of that sample and any still-relevant
   configuration, handoff, or build floor already installed by generic
   authority;
3. `WriteQuorum` succeeds only after the admitted write quorum reaches that
   invocation floor;
4. `All` succeeds only after every required admitted member reaches that
   invocation floor;
5. a cached lower floor from an earlier invocation cannot satisfy a later
   invocation whose durable local progress has advanced;
6. a "no outstanding catch-up" state may return success only if the requested
   quorum already satisfies the newly sampled invocation floor.

This strengthens the generic behavior of the existing callback without adding
an LSN parameter or changing its return type. It applies equally to the
built-in and independent custom replicators.

The host can then produce a generic, operation-bound lower-bound receipt:

1. while writes are fenced and the configuration/session fence is stable, it
   samples `current_progress` before invoking
   `wait_for_catch_up_quorum`;
2. under one compatible history, the callback's later invocation-time sample
   must be greater than or equal to the host's earlier sample; a regression
   cancels the attempt as a lineage, authority, or implementation error;
3. the host's sampled LSN is therefore a conservative lower bound covered by
   successful callback completion;
4. the host commits the exact operation, configuration, sessions, quorum mode,
   and conservative lower bound as the generic catch-up completion outcome;
5. ordinary progress refresh may report a higher raw LSN but cannot advance
   catch-up-completion or quorum credit;
6. after process restart, the host must re-run the fresh-floor callback under
   current authority before producing a new completion receipt.

In other words:

```text
host pre-call sample <= callback invocation floor <= completed quorum progress
```

All three comparisons must refer to one compatible history under an unchanged
authority, configuration, and process-session fence.

This construction is conservative. The application may establish a stronger
native barrier while the callback runs, but the generic runtime credits only
the host's earlier lower bound. If that bound is below the required handoff
LSN, convergence remains pending and a later current-authority callback may
certify a newer bound.

The host sequence is therefore:

1. while writes are fenced and the configuration/session fence is stable, the
   host samples `current_progress` before invoking
   `wait_for_catch_up_quorum`;
2. invoke the callback, which independently establishes and satisfies its
   fresh floor;
3. revalidate the authority, configuration, sessions, and compatible history;
4. commit only the conservative host sample as certified completion credit.

Convergence succeeds when all of the following are true:

1. the target completed the exact admitted PC/CC configuration operation;
2. the target's operation-bound catch-up completion lower bound is greater
   than or equal to the handoff LSN;
3. the current-configuration quorum completion lower bound from that same
   callback outcome is greater than or equal to the handoff LSN;
4. the observations belong to current admitted replica and process sessions;
5. access and role sequencing still match the admitted switchover operation.

Progress below the boundary remains insufficient. Progress above the boundary
must not be rejected merely because it is no longer equal to the historical
handoff scalar.

#### Required invariants

- Generic LSN comparison is monotonic: later compatible progress dominates an
  earlier required boundary.
- Raw progress does not prove lineage, session identity, membership, callback
  completion, or quorum credit by itself.
- A later raw progress refresh cannot rewrite or inflate a completed
  callback's certified lower bound.
- Repeated waits in one unchanged configuration cannot reuse a lower cached
  barrier when local durable progress has advanced.
- A vacuous or no-work callback success cannot certify progress beyond the
  quorum position actually established for that invocation.
- Configuration completion and quorum progress must refer to the same admitted
  transition.
- Restart may discard process-local catch-up context without losing the
  durable generic handoff requirement.
- No application is asked to persist a controller-selected historical scalar.

### 3. Promote the target before attaching the former primary

#### Current conflict

The current planned-switchover choreography processes the former primary
before the target. It requests that the old source complete an
ActiveSecondary transition and potentially regain read access before the
target has become the new primary.

A truthful PostgreSQL secondary callback cannot claim that it follows the
exact new primary while that primary does not yet exist. Returning pending
prevents the controller from reaching target promotion, creating a sequencing
cycle.

Relevant integration points include:

- `kuberic-controller/src/evaluator.rs` planned-switchover ordering;
- `kuberic-runtime/src/host/coordinator.rs` role and access effect sequencing;
- `StatefulServiceReplica::change_role`;
- `Replicator::change_role`.

#### Required behavior

Planned switchover must use this generic order:

1. close client writes on the source and drain/fence the old primary;
2. capture and durably retain the generic handoff boundary;
3. catch the target and required quorum up through that boundary;
4. revoke remaining source service authority;
5. advance epoch and promote the target;
6. while all client access remains closed, command enough replicas into the
   admitted ActiveSecondary configuration to supply its write quorum;
7. verify target primary readiness, the required live secondary membership,
   and current-configuration quorum catch-up;
8. publish target write access;
9. admit secondary read access individually only after each replica's
   secondary callbacks complete.

The former primary may remain closed and pending while the target is promoted.
Secondary attachment is not a prerequisite for target promotion once the
required handoff and recovery predicates are satisfied. It is a prerequisite
for target write publication only when the admitted configuration cannot form
its write quorum without that replica. If other admitted replicas already
supply the quorum, former-primary attachment may finish later.

The generic recipe requests only the admitted secondary role, configuration,
catch-up, and access postconditions. PostgreSQL may satisfy those callbacks by
direct follow, `pg_rewind`, or a fresh authorized build, but those native
methods do not become generic controller phases or instructions.

#### Required invariants

- The source cannot acknowledge new client writes after the handoff fence.
- The target cannot serve writes before epoch, role, configuration, catch-up,
  live write quorum, and access postconditions complete.
- The former primary cannot serve reads while it is primary-looking,
  divergent, rewinding, or not following the admitted source.
- Retry after controller, agent, or process restart resumes from durable
  generic transition state.
- No application-specific promotion or follow phase is added to generic
  controller status.

## PostgreSQL Callback Responsibilities

Once the prerequisites are available, PostgreSQL implements the generic
contract without another durable metadata owner:

- `update_epoch` stops accepting predecessor-epoch work and reconciles native
  receiver/source configuration.
- `update_catch_up_replica_set_configuration` translates generic PC/CC
  membership into native synchronous and replication configuration.
- `wait_for_catch_up_quorum` reads back native configuration, establishes the
  fresh invocation floor from current durable PostgreSQL progress, establishes
  a native barrier at or beyond that floor, and returns only after the
  requested members have replayed through it. It cannot reuse a lower
  configuration-cached barrier after local progress advances.
- `update_current_replica_set_configuration` removes predecessor membership
  only after the runtime has completed the handover recipe.
- `current_progress` and `catch_up_capability` come from PGDATA and live
  PostgreSQL rather than copied metadata.
- `build_replica` treats interrupted base backup or rewind as a failed attempt
  and starts a fresh authorized build when necessary.
- `change_role` reconstructs from PGDATA and remains pending when the admitted
  native postcondition is not yet possible.
- `on_data_loss` uses fresh survivor state, timeline compatibility, and generic
  quorum authority; it does not recover an application-private election
  journal.

Callback completion is the proof exposed to the generic runtime. For catch-up,
the runtime binds completion to the conservative pre-call lower bound described
above. The application must not return success merely because work was started
or a local configuration file was written.

## Recovery Without Historical Witness Certificates

The SF-aligned model guarantees that acknowledged data is not lost without
persisting the exact sessions that acknowledged every historical LSN:

1. Generic PC/CC and epoch authority identify the configurations that could
   have accepted writes.
2. A completed native synchronous write was durable on the required write
   quorum.
3. Recovery gathers fresh progress from a compatible responder quorum.
4. The configured recovery quorum intersects every possible write quorum.
5. Receiver/source fencing prevents observations from changing during final
   selection.
6. For every still-relevant previous or current configuration, recovery
   identifies the intersecting responder on one compatible timeline ancestry
   and carries forward its acknowledged prefix obligation.
7. Within the selected compatible history, the authoritative recovery boundary
   is the greatest durable progress reported by the final fenced responder
   set. Prefix ordering means that this copy contains every lower acknowledged
   prefix represented by an intersecting responder.
8. The selected promotion candidate must already contain or replay through
   that authoritative boundary before promotion.
9. Configuration retirement, rebuild, rewind, or storage deletion cannot
   discard the last copy carrying a still-relevant PC/CC obligation.
10. If responder histories are incompatible, an intersecting configuration
    cannot be proven, or the candidate cannot reach the selected boundary,
    recovery enters quorum loss rather than choosing a shorter compatible
    copy.
11. Promotion and access remain closed until the selected copy and required
    quorum satisfy the generic recovery recipe.

An unacknowledged suffix may survive and become visible if it belongs to the
selected compatible history. The guarantee is preservation of acknowledged
writes, not forced rollback of every write whose client lost its response.

The runtime may durably retain generic LSN boundaries produced by generic
operations, such as a handoff or build boundary. It must not retain
PostgreSQL-specific policy objects, native witness transcripts, timeline
snapshots, or application workflow phases.

## Prohibited Implementations

The prerequisites must not add:

- PostgreSQL-specific fields or branches in `kuberic-runtime`;
- PostgreSQL-specific controller evaluation or `KubericSet.status`;
- a new PostgreSQL CRD;
- application payloads, type URLs, JSON, arbitrary bytes, or extension maps in
  `AgentStore`;
- PostgreSQL policy, timeline, witness, promotion, or recovery records in
  `AgentState` or `SqliteStore`;
- a durable host relay or application report cache;
- a replacement file, embedded database, GUC, marker, or PGDATA journal for
  `state-v2.json`;
- changes to the protected public replication interfaces.

Any required generic completion receipt must have semantics applicable to
independent custom replicators and contain only existing generic authority,
operation identity, role/configuration, and LSN concepts.

## PostgreSQL Metadata-Disposition Gate

The three generic changes are necessary prerequisites, not a complete proof
that `state-v2.json` can be removed.

After the generic prerequisite gate passes, the PostgreSQL design must provide
a reviewed disposition matrix for every field in `PgDurableState` and every
nested build/recovery record. Each row must identify:

- current field or record and every production read/write site;
- target owner: PGDATA/live PostgreSQL, existing generic authority, process
  memory, disposable scratch, or deletion;
- reconstruction source after process, Pod, and full-cluster restart;
- callback or generic operation whose completion replaces any former durable
  workflow cursor;
- interruption behavior before and after irreversible PostgreSQL or filesystem
  mutation;
- stale-session, epoch, configuration, PVC/incarnation, and timeline checks;
- retention or rejection evidence needed after detailed state is discarded;
- focused crash-boundary and process-boundary tests.

The inventory must include identity and process generation, copied native
identity/progress, role/access/stopped state, synchronous policy and
generation, certified progress, accepted authority, inbound/outbound and
suspended builds, catch-up state, retired build IDs and attempts, build epochs,
and every recovery election, responder, fence, promotion, and source-follow
stage.

No code may stop reading `state-v2.json`, delete `PgDurableStore`, or remove
the application metadata root until this matrix is approved and all rows have
an implemented, tested disposition. A missing disposition blocks removal; it
does not justify a generic opaque persistence field.

## Generic Prerequisite Verification Gate

The later PostgreSQL metadata-disposition design may begin only after all
generic prerequisite behavior is covered by generic custom-replicator
fixtures.

### Cold restoration

- Restart a primary and secondary with their durable application copies
  intact but no process-local sessions.
- Prove startup remains externally closed while role restoration is pending.
- Register fresh peer sessions after listener startup.
- Prove deferred callbacks retry under the original authority and stale
  sessions cannot complete them.
- Crash or replace the host during each stage and verify fail-closed replay.

### Monotonic handoff

- Prove callback success commits the pre-call lower bound rather than a later
  raw progress sample.
- Complete an initial wait at LSN 90, advance only local durable progress to
  110, and prove a repeated wait cannot reuse the 90 barrier or report 110
  credit until the requested quorum reaches at least 110.
- Exercise the no-outstanding-catch-up path and prove it cannot vacuously
  certify a newly sampled higher floor.
- Prove ordinary progress refresh cannot inflate callback-certified or quorum
  credit.
- Accept target and quorum progress exactly equal to the handoff boundary.
- Accept compatible progress greater than the handoff boundary.
- Reject progress below the boundary.
- Reject progress from stale sessions, wrong configurations, or incompatible
  operation completions.
- Restart after catch-up and prove the generic boundary remains enforceable
  without application-local history.

### Target-first switchover

- Verify source writes close before boundary capture.
- Verify target promotion does not wait for the former primary to become a
  secondary.
- Verify a two-member configuration keeps target writes closed until the
  former primary completes the admitted secondary/configuration/catch-up
  postconditions.
- Verify a larger configuration may publish writes before former-primary
  attachment only when other admitted replicas supply the full write quorum.
- Verify the former primary remains closed until its generic secondary
  postconditions complete, regardless of whether PostgreSQL uses direct
  follow, rewind, or a fresh build.
- Crash at every ordering boundary and prove only one replica can regain write
  access.

### Recovery prefix

- Recover from a fenced responder quorum containing one shorter compatible
  candidate and one longer intersecting copy; require the selected candidate
  to reach the longer authoritative boundary.
- Cover both previous and current configuration acknowledgement obligations.
- Reject incompatible timeline branches and an unavailable authoritative
  prefix.
- Prevent retirement or rebuild from deleting the final copy carrying a
  still-relevant acknowledgement obligation.

### Regression

- Existing generic runtime/controller tests pass.
- The protected public API check passes.
- No new generic durable schema contains application-specific state.
- Existing PostgreSQL smoke tests remain green before metadata removal begins.

Representative validation commands:

```text
cargo nextest run -p kuberic-runtime -p kuberic-controller --all-features --profile ordinary
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
scripts/check_runtime_public_api.sh
just nextest-postgres-smoke
```

The full PostgreSQL suite remains opt-in and is not part of the routine
prerequisite gate.

## Implementation Order

1. Implement deferred cold restoration and its generic fixture coverage.
2. Strengthen every generic catch-up invocation to establish a fresh floor,
   add the operation-bound lower-bound receipt, and replace exact handoff
   equality with operation-bound monotonic convergence.
3. Implement target-first planned-switchover choreography.
4. Run the complete generic prerequisite gate.
5. Create and approve the complete PostgreSQL metadata-disposition matrix.
6. Rewrite and re-review the PostgreSQL restart-stateless design against the
   implemented generic behavior and approved matrix.
7. Only then begin PostgreSQL PGDATA reconstruction and staged removal of
   `state-v2.json`.

If any prerequisite requires an application-specific exception, the
prerequisite fails and PostgreSQL metadata removal remains blocked.

## Relationship to Other Designs

- [PostgreSQL Restart-Stateless Metadata](stateless-metadata.md) is blocked and
  must be rewritten around these completed prerequisites before
  implementation.
- [Service Fabric Alignment and Runtime Simplification](../kuberic/service-fabric-alignment.md)
  defines the broader generic ownership direction.
- [Stateless Default Replicator](../kuberic/stateless-default-replicator.md)
  applies the same V1 restart-reconstruction principle to the built-in
  replication engine.
- [PostgreSQL Native Replication Design](design.md) describes the current
  implementation, which still uses `state-v2.json`.
