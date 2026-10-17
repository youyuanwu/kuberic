# Service Fabric Contract Audit Tracker

## Status and Scope

Audit date: 2026-10-16.

Audited baseline: `243f622b6eacc0c30d6ba59fb909e92270531ff3`, including the
documentation-only additions to the SF-aligned traits in the working tree.
Source line references below describe that audited state; use the named
functions when subsequent changes move those lines.

Trait definitions have since moved from `application.rs` to `src/traits.rs`
and from `replicator/mod.rs` to `replicator/traits.rs`, preserving existing
public import paths. Historical evidence locations below still refer to the
audited baseline; this file-only extraction does not resolve any finding.

The audit compared the current implementation with the
[SF stateful trait documentation](https://github.com/Azure/service-fabric-rs/blob/b472b145746cb3166ec746c6d8896278bcd60e88/crates/libs/core/src/runtime/stateful_traits.rs),
the local trait contracts, and
[Kuberic's API semantics](service-fabric-api-semantics.md). It covered the
built-in replicator, runtime lifecycle and access gates, controller topology
wiring, KVStore2/SQLite providers, and the production PostgreSQL Public V1
adapter. Legacy PostgreSQL paths were not treated as evidence that the
factory-selected Public V1 path satisfies a contract.

Initial inventory: **17 implementation findings: 10 P1 and 7 P2**, plus
**one documentation/behavior mismatch**. All entries are initially open.
This initial inventory describes the historical audit, not current status.

Current resolution review: **3 fixed / 14 open implementation findings**
(8 open P1 and 6 open P2), plus the still-open documentation mismatch.
SF-04/SF-05 were fixed by PR #151; SF-14 is fixed by the local implementation
revision recorded below. Original failure traces remain historical evidence.
Only those three statuses change; in particular SF-06 and SF-15 remain open.

These findings have source-level execution traces. They are not claims of
dynamically reproduced data loss, resource exhaustion, or incorrect elections.
Those consequences remain qualified in the relevant entries.

For the API-surface comparison, see
[Stateful API and Trait Compatibility](service-fabric-alignment.md#stateful-api-and-trait-compatibility).
That comparison separates interface and architectural differences from the
implementation findings tracked here; similar method names do not imply
drop-in SF compatibility.

## Tracking Rules

P1 denotes a correctness, access-admission, required lifecycle, or
resource-retirement failure to address first. P2 denotes a narrower API
contract, availability, or cleanup gap. These are engineering priorities, not
security severity ratings.

Status values are `Open`, `In progress`, `Fixed`, `Accepted deviation`, and
`Not reproducible`. Keep IDs stable. When changing a status, record the
implementation commit or PR and regression evidence in the finding's section.
An accepted deviation requires an explicit contract decision and corresponding
documentation; changing a comment alone does not establish that serving or
durability behavior is safe.

Preserve the `Replicator`, `PrimaryReplicator`, and `StateProvider` interfaces
while addressing findings. SF-04 and SF-05 are independent sequential blockers:
fixing capability admission does not fix swap staging.

## Inventory

| ID | Priority | Status | Finding |
|---|---|---|---|
| [SF-01](#sf-01-applied-kv-state-is-visible-before-the-read-fence) | P1 | Open | Applied KV state is visible before the read fence |
| [SF-02](#sf-02-kv-writes-bypass-partition-write-denial) | P1 | Open | KV writes bypass partition write denial |
| [SF-03](#sf-03-postgresql-sql-admission-ignores-partition-denial) | P1 | Open | PostgreSQL SQL admission ignores partition denial |
| [SF-04](#sf-04-controller-swap-capability-conflicts-with-runtime-admission) | P1 | Fixed | Controller swap capability conflicts with runtime admission |
| [SF-05](#sf-05-swap-installs-the-final-role-layout-before-source-catch-up) | P1 | Fixed | Swap installs the final role layout before source catch-up |
| [SF-06](#sf-06-access-revocation-can-block-teardown-and-violate-role-ordering) | P1 | Open | Access revocation can block teardown and violate role ordering |
| [SF-07](#sf-07-idle-postgresql-removal-globally-drains-the-partition) | P1 | Open | Idle PostgreSQL removal globally drains the partition |
| [SF-08](#sf-08-retired-postgresql-grants-leak-persistent-slots) | P1 | Open | Retired PostgreSQL grants leak persistent slots |
| [SF-09](#sf-09-postgresql-epoch-fencing-regresses-reported-progress) | P1 | Open | PostgreSQL epoch fencing regresses reported progress |
| [SF-10](#sf-10-quiet-snapshot-builds-fail-boundary-proof) | P1 | Open | Quiet snapshot builds fail boundary proof |
| [SF-11](#sf-11-suffix-reconciliation-skips-a-missing-committed-prefix) | P2 | Open | Suffix reconciliation skips a missing committed prefix |
| [SF-12](#sf-12-minority-rejection-makes-specific-quorum-catch-up-order-dependent) | P2 | Open | Minority rejection makes specific-quorum catch-up order-dependent |
| [SF-13](#sf-13-postgresql-writequorum-ignores-the-required-peer) | P2 | Open | PostgreSQL WriteQuorum ignores the required peer |
| [SF-14](#sf-14-built-in-operation-streams-are-disconnected-from-delivery) | P2 | Fixed | Built-in operation streams are disconnected from delivery |
| [SF-15](#sf-15-kv-close-does-not-join-owned-consumers) | P2 | Open | KV Close does not join owned consumers |
| [SF-16](#sf-16-postgresql-abort-leaves-gateway-grants-armed) | P2 | Open | PostgreSQL Abort leaves gateway grants armed |
| [SF-17](#sf-17-dynamic-open-leaves-settings-guards-inconsistent) | P2 | Open | Dynamic Open leaves settings guards inconsistent |
| [SF-D01](#sf-d01-catch-up-capability-is-a-tail-not-a-retained-floor) | Documentation | Open | Catch-up capability is a tail, not a retained floor |

## P1 Findings

### SF-01: Applied KV state is visible before the read fence

**Contract:** Applied-only state must not become readable as though it were
committed; partition read readiness must protect the application's read view.

**Failure:** Primary replication calls provider `apply` before publishing the
applied/committed disagreement that closes read access. KV application changes
its visible map inside `apply`. A concurrent read can pass the still-granted
gate and see the new value before peer quorum work starts. The analogous
post-apply ordering also exists on secondary ingress. A provider with a separate
committed read view can avoid this exposure; KV does not have that protection.

**Evidence:** Built-in `replicate` at
`kuberic-runtime/src/replicator/stateless/mod.rs:2839-2862`, with peer work
starting at `2880-2895`; secondary apply/publication at `1024-1084`.
KV visibility is in `examples/kvstore2/src/persistence.rs:344-359` and
`examples/kvstore2/src/service.rs:68-77`. Host fencing follows progress
publication at `kuberic-runtime/src/host/hosting.rs:917-953`.

**Closure:** Hold a barrier after visible provider mutation but before apply
returns, with a required peer unavailable. Normal primary and secondary reads
must not return the uncommitted value. Also verify commitment restores the
intended read access.

### SF-02: KV writes bypass partition write denial

**Contract:** Client writes must honor partition write readiness independently
of internal replicator role and epoch authority.

**Failure:** The HTTP write path forwards directly to replication. Neither
`replicate_put` nor the built-in replicator checks partition write status.
Host-only access revocation can therefore leave a still-configured primary
able to commit new client writes while the partition reports writes denied.

**Evidence:** `examples/kvstore2/src/main.rs:201-213` and
`examples/kvstore2/src/service.rs:57-66`; built-in internal authority checks
at `kuberic-runtime/src/replicator/stateless/mod.rs:675-690,727-767`.
Host revocation at `kuberic-runtime/src/host/hosting.rs:578-587` changes
access state without changing engine generation or authority.

**Closure:** Revoke only partition write access on a ready KV primary. An
ordinary HTTP PUT must fail without provider application or commitment.
Repeat while mandatory post-revocation catch-up is pending.

### SF-03: PostgreSQL SQL admission ignores partition denial

**Contract:** Native SQL reads and writes must follow partition admission,
including final secondary catch-up and subsequent access revocation.

**Failure:** The Public V1 native owner grants access from local lifecycle and
native readiness without consulting partition read/write status. Its cached
lifecycle context can miss status-only revocations. A secondary receiver can
become ready by those checks before final catch-up is complete.

**Evidence:** `examples/postgres/src/public_v1.rs:396-453,547-605`, especially
the cached-context return at `552-553`, and receiver admission at
`1548-1562,1591-1602`. The production factory supplies no state provider
(`examples/postgres/src/service.rs:187-198,249-250,277-291`); the host uses
committed zero without a provider and compares it with current progress
(`kuberic-runtime/src/host/hosting.rs:499-508,529-536`).

**Qualification:** A custom replicator without a state provider is permitted;
the remedy is not to invent a dummy provider. Legacy adapter access checks do
not protect this production dispatch.

**Closure:** Use the production factory with a real partition handle. Verify
SQL remains denied before admission, on status-only revocation, and on a bound
secondary whose final catch-up has not completed.

### SF-04: Controller swap capability conflicts with runtime admission

**Contract:** Captured swap capability and selected quorum mode must agree with
the attached replicator's supported behavior and runtime admission.

**Failure:** The controller captures `catch_up_specific_quorum: false` and
selects `All`, but the built-in factory advertises `true` without a downgrade.
The host requires exact equality and rejects the swap before callbacks.

**Evidence:** `planned_swap_program` and accepted evidence at
`kuberic-controller/src/evaluator/public_v1_topology.rs:690-720`;
`validate_intent_capabilities` at
`kuberic-runtime/src/host/public_lifecycle.rs:96-110`; built-in capability
construction at `kuberic-runtime/src/replicator/stateless/mod.rs:343-353`.

**Closure:** Execute a real planner-produced swap through a hosted built-in
bundle. Conservative `All` selection must have coherent capability metadata
and pass admission. A fake that manufactures operation completions is not
sufficient. Retain a separate regression for SF-05.

**Resolution — Fixed:** [PR #151](https://github.com/youyuanwu/kuberic/pull/151),
merge revision `05f69bee820cc6393842d34b45ae82e2ced32981`.
The evaluator now reads the actual optional source report capability, waits
if absent, selects true/WriteQuorum or false/All, and marks the successor
required for specific quorum
(`kuberic-controller/src/evaluator/public_v1_topology.rs:334-361`).
Runtime exact capability admission is not relaxed. The built-in regression
path is **true/WriteQuorum**, not a downgrade to false/All.

**Regression evidence:** The five real-stack journeys listed under SF-05
passed in ordinary run `7193207a-f265-4c5c-856c-17e517072aec` (331/331).
Their shared real hosted KV/built-in path explicitly asserts `Some(true)`
(`kuberic-level-tests/src/in_process/journeys.rs:1253-1269`), submits the
production planner's swap, and completes data-preserving handoff. This is
not a manufactured controller completion. No executed capability-false
hosted swap is claimed by this resolution.

### SF-05: Swap installs the final role layout before source catch-up

**Contract:** The source primary must finish mandatory post-revocation catch-up
before its replicator and application demote.

**Failure:** The planner uses the final configuration, already naming the
source ActiveSecondary, as the refreshed configuration. The host installs it
before SecondCatchUp and before source demotion. The built-in engine rejects
the mismatch with its still-Primary role. Merely relaxing that comparison is
insufficient: catch-up also requires the admitted source to remain primary.

**Evidence:** `kuberic-controller/src/evaluator/public_v1_topology.rs:712-719,783-799`;
the swap instruction order at
`kuberic-runtime/src/host/public_lifecycle_program.rs:20-39`; engine checks
at `kuberic-runtime/src/replicator/stateless/mod.rs:2701-2711,2793-2800,727-767`.

**Closure:** After resolving SF-04, prove the source can install a coherent
staging configuration and perform second catch-up with writes denied, then
demote and activate the successor in the final configuration.

**Resolution — Fixed:** [PR #151](https://github.com/youyuanwu/kuberic/pull/151),
merge revision `05f69bee820cc6393842d34b45ae82e2ced32981`.
The evaluator preserves the source Primary in an intermediate descriptor,
completes first catch-up, and prepares peer epoch/role/configuration before
issuing source Swap
(`kuberic-controller/src/evaluator/public_v1_topology.rs:238-361`).
This permits final source catch-up before source demotion; it does not merely
relax the engine's role comparison.

**Regression evidence:** All five existing
`kuberic-level-tests` `in_process::journeys::` cases passed in final serial
ordinary run `7193207a-f265-4c5c-856c-17e517072aec`:

- `specification_switchover_preserves_data_and_fences_old_writer`
- `switchover_recreation_after_catch_up_resumes_accepted_handoff`
- `switchover_recreation_after_durable_handoff_resumes_promotion`
- `switchover_cancellation_after_peer_preparation_finishes_frozen_request`
- `switchover_retarget_after_peer_preparation_serializes_frozen_requests`

The shared journey commits an additional real write after first catch-up,
proves the prepared target lacks it, and then verifies transfer of that
missing committed prefix, unchanged host sessions, old-writer rejection,
and a new-primary write
(`kuberic-level-tests/src/in_process/journeys.rs:1253-1409`).
Restart and edited-request cuts retain exact accepted completion
(`:1097-1249`). Successful switchover does **not** prove that a blocked or
failing Revoke application callback reaches Abort; SF-06 remains open.

### SF-06: Access revocation can block teardown and violate role ordering

**Contract:** Abort must not depend on graceful application work; failed Close
must reach abort containment. Actual application role changes follow the
corresponding replicator role change.

**Failure:** Close and Abort first execute Revoke, which awaits application
`change_role(None)` with error propagation. A failure skips later teardown;
a hung callback prevents Abort from being reached. During swap, the same
instruction sends application None while the replicator remains Primary for
final catch-up. These are two harms from the same access/role conflation.

**Evidence:** Recipes and Revoke at
`kuberic-runtime/src/host/public_lifecycle_program.rs:20-43,436-471`.
`kuberic-runtime/src/host/operation.rs:245-263` records failed containment
without invoking abort. Separate access fencing already occurs at
`kuberic-runtime/src/host/service.rs:680-690`.

**Closure:** Erroring and indefinitely blocked application None callbacks must
not prevent terminal containment. Record both objects' roles during swap:
access revocation must not synthesize an unpaired application None transition,
and actual application demotion must follow replicator demotion.

### SF-07: Idle PostgreSQL removal globally drains the partition

**Contract:** Removing a completed idle nonvoter releases that replica's
resources without stopping unrelated voting replicas.

**Failure:** `public_remove` invokes global revoke/drain. Removing idle B from
healthy primary P with active secondary A stops P, disconnects other peers,
and clears configurations. The exact Remove recipe does not reinstall them.
This scenario does not remove an active member or race an in-progress build.

**Evidence:** `examples/postgres/src/public_v1.rs:1648-1658,503-544`;
missing-configuration checks at `565-568,859-881`; the host's Remove-only
recipe at `kuberic-runtime/src/host/public_lifecycle_program.rs:40-41,421-429`.

**Closure:** Build idle B on live P+A, remove B twice, and retain P's SQL
connection, A's streaming, and installed configurations without restarting or
reconfiguring P. Only B's resources should disappear.

### SF-08: Retired PostgreSQL grants leak persistent slots

**Contract:** Failed, removed, or retired replica grants must eventually release
their persistent replication resources.

**Failure:** A source creates a persistent physical slot before later build
steps succeed. Canceled grants and global cleanup drain gateways but do not
retire slots. Repeated fresh target incarnations can exhaust the configured
32-slot budget; abandoned slots can retain WAL under continued writes.
Exhaustion and disk growth were inferred, not measured.

**Evidence:** Slot creation at `examples/postgres/src/public_v1.rs:959-969`;
grant cleanup at `330-340,412-443,1385-1400` and global drain at `503-537`.
Slot retirement exists in `examples/postgres/src/native.rs:198-249,861-897`,
but its adapter caller is on the legacy path. The budget is configured at
`examples/postgres/src/config.rs:176-177`.

**Closure:** Inspect `pg_replication_slots` and retained WAL across failed
builds, idle removal, active-peer omission, replacement, and restart. Exercise
more than 32 sequential fresh-incarnation attempts. Preserve slots belonging
to still-authorized peers during ordinary reconnects.

### SF-09: PostgreSQL epoch fencing regresses reported progress

**Contract:** Fencing old-epoch traffic must preserve accurate reporting of the
replica's highest contiguous durable progress for recovery and selection.

**Failure:** A live secondary reports native received/replayed position Y.
An increasing epoch drains PostgreSQL without persisting a final progress
snapshot. The stopped query then returns older metadata X. A caller relying
on the later values could misorder survivors; an incorrect election or
acknowledged-data loss was not reproduced.

**Evidence:** Live and stopped returns at
`examples/postgres/src/public_v1.rs:1275-1304,1251-1255`; epoch drain at
`812-832,503-535`. Snapshot machinery is in
`examples/postgres/src/native.rs:718-739`, with build/admission calls rather
than a final drain snapshot.

**Closure:** Advance a secondary beyond its admission snapshot, measure native
and public progress, fence to a newer epoch, and requery before restart.
Assert no regression and no access grant. Repeat None/restart boundaries with
two survivors whose post-admission tails differ.

### SF-10: Quiet snapshot builds fail boundary proof

**Contract:** A valid copied snapshot can become active and catch up without
manufacturing the original operation record at its snapshot boundary.

**Failure:** Build S through committed LSN 1 without concurrent writes.
Copy transfers no retained operation 1, but final catch-up resends operation 1
as live proof. Duplicate verification requires the missing retained record
and requests a rebuild. A provider restoring the exact record can mask this;
the valid snapshot-only provider contract does not require it.

**Evidence:** Copy range and completion at
`kuberic-runtime/src/replicator/stateless/mod.rs:1684-1696,2102-2136`;
boundary proof at `1384-1414`; duplicate verification at `1032-1038,802-805`.
The snapshot-only fixture is in
`kuberic-runtime/tests/public_v1_recovery.rs:443-457,489-503`.

**Closure:** Complete a nonzero build with no concurrent or subsequent write,
activate it, and perform both catch-up modes without rebuilding. Exercise
known progress and the already-known-peer `INVALID_LSN` sentinel while
retaining exact history, session, and storage validation.

## P2 Findings

### SF-11: Suffix reconciliation skips a missing committed prefix

**Failure:** With primary applied/committed progress (11,10), compatible peer C
at (5,5), and retained operations 6-11 available, suffix reconciliation sends
11 before repairing 6-10. C correctly rejects the gap, which is classified as
incompatible history requiring rebuild.

**Evidence:** `kuberic-runtime/src/replicator/stateless/mod.rs:1435-1443,1325-1329`;
gap rejection at `1012-1017` and rebuild conversion at `1343-1344`.

**Closure:** Repair the compatible committed prefix, then verify and settle the
suffix without replacement. Continue rejecting genuinely mismatching or
unavailable retained history.

### SF-12: Minority rejection makes specific-quorum catch-up order-dependent

**Failure:** In P/B/C with quorum two and required successor B, B's proof can
satisfy readiness. A dispensable C's remote rebuild rejection arriving first
nevertheless fails the wait; B arriving first succeeds.

**Evidence:** `kuberic-runtime/src/replicator/stateless/mod.rs:1433-1479`
returns the rebuild error at `1454` before checking the qualifying witness set.

**Closure:** Control both response orders and require equivalent success once
the healthy set satisfies both configurations and the required successor.
Local-source failures and failures of indispensable peers must remain fatal.

### SF-13: PostgreSQL WriteQuorum ignores the required peer

**Failure:** Accepted ordinary WriteQuorum catch-up checks aggregate native
certified progress but not `must_catchup`. Another standby can satisfy the
aggregate while the marked successor remains behind.

**Evidence:** `examples/postgres/src/public_v1.rs:1183-1228`, especially
`1218-1224`; aggregate policy/certification in
`examples/postgres/src/native.rs:797-857,930-1014`. Ordinary CatchUp accepts
the mode at `kuberic-runtime/src/protocol/public_operations.rs:870-873`.

**Qualification:** PostgreSQL advertises specific-quorum capability false, and
its capability-false swap path selects All. This finding does not assert that
those swaps incorrectly select WriteQuorum.

**Closure:** Either honor the marked peer for accepted ordinary input or reject
unsupported input explicitly. Pause only the marked peer while another
advances; retain separate capability-false swap-to-All coverage.

### SF-14: Built-in operation streams are disconnected from delivery

**Failure:** Factory-returned copy and replication streams never receive
transport operations. Their producers are retained only for shutdown; ingress
applies directly to `DurableState`. Direct persistence can be durable, but it
does not fulfill the promised service-stream delivery/ACK interface.

**Evidence:** `ServiceStreams` at
`kuberic-runtime/src/replicator/stream.rs:133-170`; getters at
`kuberic-runtime/src/replicator/stateless/mod.rs:2969-2974`; direct ingress
at `1024-1029,1130-1147,2079-2115,2242-2259`.

**Closure:** Prove delivery and service-ACK-dependent completion through streams
obtained from a real built-in factory. Alternatively, explicitly resolve and
document the supported direct-persistence contract rather than returning
successful permanently empty delivery handles. Preserve protected interfaces.

**Resolution — Fixed:** Local production revision
`284a46fce46e0c94d6b3255c75e24273c79e7c06`
(`fix(runtime): honor service operation delivery and durable acknowledgements`).
Built-in ingress now delivers through `ServiceStreams` and waits for durable
service ACK (`kuberic-runtime/src/replicator/stream.rs:314-329`;
`kuberic-runtime/src/replicator/stateless/mod.rs:1073-1136,2120-2327`).
CopyComplete, including empty snapshots, is explicit; retained catch-up
requires acknowledged contiguous LSNs and an exact terminal boundary.
Post-ACK checks verify actual durable progress and exact payload/history
under current authority. Queueing or physical persistence before explicit
ACK cannot complete ingress or publish successful receiver progress.

The optional service-invoked `apply_to`/`execute` helpers
(`replicator/stream.rs:72-160`) cover apply, duplicate-byte verification,
requested commitment, cancellation and actual-effect drainage. KV/SQLite
consumers explicitly ACK/reject those results
(`examples/kvstore2/src/service.rs:216-233`;
`examples/sqlite/src/service.rs:347-360`). There is no automatic factory
consumer or double-apply receiver fallback. Primary-local apply/commit
ownership is unchanged.

**Regression evidence:** Eleven real hosted factory/service tests in
`kuberic-runtime/tests/public_v1_recovery.rs:1463-2328` passed in final
ordinary run `7193207a-f265-4c5c-856c-17e517072aec`:

- `hosted_operation_stream_copy_and_completion_wait_for_durable_ack`
- `hosted_operation_stream_replication_waits_for_durable_ack`
- `hosted_operation_stream_duplicate_commit_advancement_reaches_service`
- `hosted_operation_stream_reject_drop_and_invalid_ack_do_not_complete`
- `hosted_operation_stream_invalid_history_and_copy_complete_acks_are_fenced`
- `hosted_operation_stream_caller_cancellation_fences_queued_and_delivered_operations`
- `hosted_operation_stream_authority_changes_cancel_effects_and_fence_late_ack`
- `hosted_operation_stream_successor_and_close_wait_for_synchronous_effects`
- `hosted_operation_stream_epoch_and_configuration_cancel_active_persistence`
- `hosted_operation_stream_close_and_abort_drain_without_a_consumer`
- `hosted_operation_stream_primary_local_apply_is_not_redelivered`

These exercise actual delivery and held durable ACKs, not send-only channels
or fabricated factory completion. They prove exact retry after seven
reject/drop/invalid ACK outcomes, reject skipped retained LSNs and corrupted
durable bytes, check access/report fencing, and observe real synchronous
effect drainage. MemoryState proves sequencing, not fsync.
Real KV/SQLite `stream_replication_commits_without_reapplying_duplicate_payload`
tests count provider apply/commit calls and inspect persisted records
(`examples/kvstore2/src/service.rs:375-451`;
`examples/sqlite/src/service.rs:420-474`).
SQLite `stream_cancellation_after_durable_apply_does_not_commit` verifies
cancellation plus stream closure against the real paused provider (`:369-415`).
All passed in the targeted 52-test recovery/provider/stream run and the final
331-test ordinary run. Cancellation does not promise rollback; this closure
does not resolve the distinct SF-15 trait-Close task-join finding.

### SF-15: KV Close does not join owned consumers

**Failure:** Trait Close signals cancellation and clears references but retains
unjoined consumer tasks. Only the separate concrete `shutdown_and_drain`
helper joins them, after the whole host terminates. Close itself is therefore
not the documented graceful descendant-drain boundary.

**Evidence:** `examples/kvstore2/src/service.rs:173-181,87-103,307-325`;
host trait Close at
`kuberic-runtime/src/host/public_lifecycle_program.rs:436-441`; later helper
invocation at `examples/kvstore2/src/main.rs:154-160`.

**Closure:** Pause an owned consumer mid-operation. Trait Close must retain
cancellation-safe join ownership and remain pending until the consumer
terminates. This is distinct from SF-14's delivery-wiring problem.

### SF-16: PostgreSQL Abort leaves gateway grants armed

**Failure:** Abort cancels the lifecycle owner and reaps PostgreSQL but does not
cancel retained grant/gateway tokens. Keeping service/driver references alive
can therefore retain listeners and tasks until a later Close or Drop. This
does not mean the stopped backend continues committing SQL.

**Evidence:** `examples/postgres/src/adapter.rs:1250-1254` and
`examples/postgres/src/service.rs:89-92,233-236`; owner exit at
`examples/postgres/src/public_v1.rs:396-404`; independent gateway tokens at
`examples/postgres/src/public_transport.rs:58-61,86-117,128-132`.
Gateway validity inputs at `public_v1.rs:979-990` are not invalidated by
`examples/postgres/src/instance.rs:1528-1536`.

**Closure:** Retain the objects, invoke each Abort entry without Close/Drop,
and verify listeners, connections, tasks, and retained references are
reclaimed. Separately reconcile prompt-return semantics with required
exact-process reaping; a Close-style graceful wait is not a substitute for
gateway containment.

### SF-17: Dynamic Open leaves settings guards inconsistent

**Failure:** Registering `http://host:0` and opening to a resolved port changes
the engine address but not the outer registration-bound address. Reapplying
the resolved endpoint fails the outer guard; reapplying port zero fails the
inner guard. Current impact is limited because settings expose only the
immutable address.

**Evidence:** Registration/wrapper guards at
`kuberic-runtime/src/replicator/mod.rs:543-578,615-620,1172-1178`; resolution
and inner guard at
`kuberic-runtime/src/replicator/stateless/mod.rs:2332-2341,2977-2986`.

**Closure:** Through a public port-zero factory/host bundle, reapply unchanged
effective settings successfully without rebinding or changing identity.
Continue rejecting a genuinely different endpoint.

## Documentation and Contract Reconciliation

### SF-D01: Catch-up capability is a tail, not a retained floor

**Mismatch:** The contract describes the earliest retained catch-up progress,
but the built-in implementation returns `state.committed_lsn`. A log retaining
operations 1-10 can report 10 even though an intermediate peer can catch up.
The actual catch-up range uses peer current progress, not this capability.

**Evidence:** `kuberic-runtime/src/replicator/mod.rs:173-178` and
`kuberic-runtime/src/replicator/stateless/mod.rs:2532-2541,1384-1392`.

**Closure:** Decide and document the scalar's meaning, including empty,
snapshot-only, and pruned history and `INVALID_LSN`. Align behavior or record
an intentional conservative deviation. Returning a conservative tail is not,
by itself, evidence of unsafe quorum acknowledgement or data corruption.

## Audit Boundaries and Intentional Differences

The audit did not count async signatures, omission of COM cancellation tokens,
explicit primary epoch replay, or exact Public V1 identity/session/storage
authority as violations merely because they differ from SF.

Other distinctions remain explicit rather than being counted as extra findings:

- Built-in applied progress during access-closed recovery is not automatically
  unsafe primary committed progress. Clarify the recovery wording separately
  from SF-01's visibility interval.
- PostgreSQL physical copy is not a deterministic StateProvider byte stream;
  byte differences across base backups alone do not prove a copy safety bug.
- PostgreSQL's conservative PC/CC synchronous policy can deny some otherwise
  satisfiable configurations, but successful policy satisfies both quorums.
- Frozen per-call PostgreSQL catch-up boundaries versus moving committed
  targets need a contract decision; no additional unsafe-wait finding is
  asserted here.
- Immediate Abort return versus proven native-process reaping needs an explicit
  policy. Neither choice excuses SF-16's retained gateways.
- Fresh-replacement restrictions, callback concurrency, and full controller
  reconfiguration safety were not exhaustively proved by this audit.

## Verification Record

### Original Audit

The audit ran these existing targeted selections successfully:

```sh
cargo test --locked -p kvstore2 service::tests::
cargo test --locked -p kuberic-runtime --features testing \
    --test public_v1_recovery \
    hosted_public_v1_contained_close_restarts_without_replaying_interrupted_children \
    -- --exact
```

The first ran five tests in each KV library/binary target and confirms that
Close retains tasks for a separately invoked drain. The second ran one hosted
close-recovery test and covers fallback after a child Close error, not the
earlier Revoke-error path in SF-06. Passing these selections does not close
the findings or dynamically reproduce the other failure scenarios.

No real PostgreSQL or Kubernetes run was performed for the audit. Routine
PostgreSQL verification should use `just nextest-postgres-smoke`; the full
`just nextest-postgres` suite requires an explicit request. Add focused
regressions for each closure condition rather than using the existing
conformance catalog or controller completion fakes as blanket proof.

### Resolution Verification

Final state, run `7193207a-f265-4c5c-856c-17e517072aec`:

```sh
cargo nextest run -p kuberic-runtime -p kuberic-controller \
    -p kuberic-level-tests -p kvstore2 -p sqlite-replicated \
    --all-features --profile ordinary --test-threads 1
```

**331 passed, 4 skipped**, 555.581 seconds, including all named regressions
above. Final targeted recovery/provider/stream selection: **52 passed**
(`c3ed69bd-ac9a-4383-8f26-0e65f46f580f`).
Workspace format/check/all-features clippy with warnings denied, doctests
(2), source API inventory (4), privacy (1), and executable built-in
conformance (1; `just nextest-build-only`) passed.

The approved `just nextest-postgres-smoke` passed **8 tests** (run
`00cd4814-b216-4cdf-b3c7-3437d1c27aac`, 116.480 seconds) with
`KUBERIC_POSTGRES_PUBLIC_V1_TMPDIR=/tmp`. An initial long checkout-local socket
root failed all eight before readiness; the standard short root resolved
that environment limit. Earlier ordinary iterations exposed retry, retained
boundary, and primary-report regressions; they were corrected before the
final passing runs. These results do not close unrelated PostgreSQL findings.

Exact inventory validation measured **377 = 331 ordinary + 41 PostgreSQL +
3 KinD live + 2 build-only**; all tier lists and configured partition unions
passed. Full PostgreSQL, live KinD, and archive rebuild were not executed.
SF-14's implementation revision is local; no push, PR, or merge was performed
as part of this resolution.
