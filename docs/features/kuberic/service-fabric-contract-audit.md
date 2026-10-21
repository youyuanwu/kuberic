# Service Fabric Contract Audit Tracker

> Historical baseline and finding register. The connected classic V1 cutover
> removes the audited bespoke engine; [Classic V1 Runtime](classic-v1-runtime.md)
> describes current ownership and APIs. Original traces and source references
> remain historical evidence, not current paths or automatic audit closure.

## Status and Scope

Audit date: 2026-10-16.

SF trait-semantics follow-up: 2026-10-17, against Kuberic
`e81e6c3f7cce36df1e0d0107040034e38d3acad7` and
[`mssf-ext` traits at `615da9a`](https://github.com/youyuanwu/service-fabric-apps-rs/blob/615da9a886d0d1d68af63f9c3916024986309673/crates/libs/mssf-ext/src/traits.rs).
SF-18 through SF-21 use this follow-up baseline; earlier source references
retain their historical baseline below.

Lifecycle/control-plane follow-up: 2026-10-17, against Kuberic
`1496fd8a21c25ec4c53257b633b1d8f0f3281e18` and
[`service-fabric-rs` stateful traits at `7c1ea4e`](https://github.com/Azure/service-fabric-rs/blob/7c1ea4e5f9ab5fbe4cfa0b3e9bfaf37169d2b43f/crates/libs/core/src/runtime/stateful_traits.rs).
This comparison adds SF-D02 and clarifies known-replica `-1` configuration
input. The upstream primary-replicator comments identify themselves as
unofficial and subject to correction. That file does not define
`StateProvider`, `StateReplicator`, or operation-stream/ACK interfaces;
SF-18 through SF-21 retain their separate `mssf-ext` evidence.

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

Pre-P6 recorded inventory (historical): **21 implementation findings: 7 fixed / 14 open**
(5 open P1 and 9 open P2), plus **one open documentation/behavior
mismatch** (SF-D01). SF-D02 is fixed independently of implementation counts.
SF-04/SF-05 were fixed by PR #151; SF-14 is fixed by the implementation
published in PR #154 and recorded below. Original failure traces remain
historical evidence. SF-18 through SF-21 track the remaining stream, context,
primary-completion, and acknowledgement alignment requirements. SF-06 is
fixed by local commits `ac16f97` and `d73db99`, as recorded below; SF-15
was open at that checkpoint.
SF-01 is fixed by local commit `434cd18`; SF-11's pre-existing prefix repair
in `05f69bee` (PR #151) is now revalidated by an exact regression in
`434cd18`. No additional SF-11 runtime change was needed.
SF-10 is fixed by local commits `53038e3` and `dbceb59`, with source-backed native comparison
and quiet snapshot-only regressions recorded below.

P6 current dispositions are qualified by the
[finding-to-current-code-to-regression matrix](sf-v1-p6-closure.md). SF-03,
SF-07/08/09/16 and SF-15 are fixed within their selected application/custom
paths. SF-13's silent-success defect is fixed by explicit unsupported rejection
of marked WriteQuorum, not an implementation of that capability. SF-17's bounded
current-factory case is fixed, while the broader P7 settings obligation remains
open. These changes use actual production-path regressions and the implementation
commits below; unrelated historical findings are not automatically reclassified.

**Alignment decision:** The user requested alignment with the linked SF
semantics on 2026-10-17. SF-18 through SF-21 are required changes, not accepted
Kuberic deviations or documentation-only differences. SF-14's closure is
limited to real service delivery and ACK-dependent completion; it does not
establish full SF trait-semantic compatibility.

These findings have source-level execution traces or interface-contract
comparisons. They are not claims of dynamically reproduced data loss,
resource exhaustion, or incorrect elections. Those consequences remain
qualified in the relevant entries.

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

### Coordinated Semantic-Alignment Roadmap

The [faithful SF V1 Rust async/tonic port plan](sf-v1-semantic-alignment-plan.md)
supersedes choosing the next implementation solely from this inventory or
repairing the bespoke algorithm one item at a time.
Native-source research covers additional differences in catch-up targets,
receive-versus-service-ACK progress, access timing, epoch callbacks and outer
Close outcomes. They are explicit compatibility obligations in that roadmap,
not closures or retroactive renumbering of these historical findings.

Port native queue/session and primary/secondary/copy state machines first,
prepare service/host adapters, then atomically switch the engine, public API,
reports/controller and lifecycle while removing the old path. SF-18 through
SF-21, SF-D01 and other gaps are closure checks against that port, not independent
replacement algorithms. Open application/custom-replicator findings remain
mapped to later phases. Fixed findings retain their historical evidence,
but affected contracts (including SF-01/SF-D02/SF-10) must be requalified rather
than treated as proof of native equivalence. That historical documentation-only planning update made no status/count changes;
the subsequent P6 implementation dispositions below are separately evidenced.

| ID | Priority | Status | Finding |
|---|---|---|---|
| [SF-01](#sf-01-applied-kv-state-is-visible-before-the-read-fence) | P1 | Fixed | Applied KV state is visible before the read fence |
| [SF-02](#sf-02-kv-writes-bypass-partition-write-denial) | P1 | Open | KV writes bypass partition write denial |
| [SF-03](#sf-03-postgresql-sql-admission-ignores-partition-denial) | P1 | Fixed (P6 scoped) | PostgreSQL SQL admission ignores partition denial |
| [SF-04](#sf-04-controller-swap-capability-conflicts-with-runtime-admission) | P1 | Fixed | Controller swap capability conflicts with runtime admission |
| [SF-05](#sf-05-swap-installs-the-final-role-layout-before-source-catch-up) | P1 | Fixed | Swap installs the final role layout before source catch-up |
| [SF-06](#sf-06-access-revocation-can-block-teardown-and-violate-role-ordering) | P1 | Fixed | Access revocation can block teardown and violate role ordering |
| [SF-07](#sf-07-idle-postgresql-removal-globally-drains-the-partition) | P1 | Fixed (P6 scoped) | Idle PostgreSQL removal globally drains the partition |
| [SF-08](#sf-08-retired-postgresql-grants-leak-persistent-slots) | P1 | Fixed (P6 ownership) | Retired PostgreSQL grants leak persistent slots |
| [SF-09](#sf-09-postgresql-epoch-fencing-regresses-reported-progress) | P1 | Fixed (orderly observation) | PostgreSQL epoch fencing regresses reported progress |
| [SF-10](#sf-10-quiet-snapshot-builds-fail-boundary-proof) | P1 | Fixed | Quiet snapshot builds fail boundary proof |
| [SF-11](#sf-11-suffix-reconciliation-skips-a-missing-committed-prefix) | P2 | Fixed | Suffix reconciliation skips a missing committed prefix |
| [SF-12](#sf-12-minority-rejection-makes-specific-quorum-catch-up-order-dependent) | P2 | Open | Minority rejection makes specific-quorum catch-up order-dependent |
| [SF-13](#sf-13-postgresql-writequorum-ignores-the-required-peer) | P2 | Fixed (explicit rejection) | PostgreSQL WriteQuorum ignores the required peer |
| [SF-14](#sf-14-built-in-operation-streams-are-disconnected-from-delivery) | P2 | Fixed | Built-in operation streams are disconnected from delivery |
| [SF-15](#sf-15-kv-close-does-not-join-owned-consumers) | P2 | Fixed (service trait Close) | KV Close does not join owned consumers |
| [SF-16](#sf-16-postgresql-abort-leaves-gateway-grants-armed) | P2 | Fixed (retained gateways) | PostgreSQL Abort leaves gateway grants armed |
| [SF-17](#sf-17-dynamic-open-leaves-settings-guards-inconsistent) | P2 | Open (bounded factory case fixed) | Dynamic Open leaves settings guards inconsistent |
| [SF-18](#sf-18-copy-stream-lifetime-prevents-sf-sequential-consumption) | P2 | Open | Copy stream lifetime prevents SF sequential consumption |
| [SF-19](#sf-19-copy-context-comes-from-the-primary-instead-of-the-secondary) | P2 | Open | Copy context comes from the primary instead of the secondary |
| [SF-20](#sf-20-primary-replication-lacks-the-sf-early-lsn-and-local-commit-pipeline) | P2 | Open | Primary replication lacks the SF early-LSN and local-commit pipeline |
| [SF-21](#sf-21-acknowledgement-requires-kuberic-progress-and-always-explicit-ack) | P2 | Open | Acknowledgement requires Kuberic progress and always-explicit ACK |
| [SF-D01](#sf-d01-catch-up-capability-is-a-tail-not-a-retained-floor) | Documentation | Open | Catch-up capability is a tail, not a retained floor |
| [SF-D02](#sf-d02-primary-progress-promise-exceeds-the-built-in-query-result) | Documentation | Fixed | Primary progress promise exceeds the built-in query result |

## P1 Findings

### SF-01: Applied KV state is visible before the read fence

**Contract:** For Kuberic's built-in engine, close normal read admission before
provider work can expose an applied-only mutation. This is a conservative
Kuberic admission guarantee, not a claim that Service Fabric partition status
isolates every application read/view. Already-admitted reads and raw provider
access require service-owned isolation; see
[read visibility qualifications](service-fabric-api-semantics.md#read-visibility-and-admission).

**Historical failure:** Primary replication calls provider `apply` before publishing the
applied/committed disagreement that closes read access. KV application changes
its visible map inside `apply`. A concurrent read can pass the still-granted
gate and see the new value before peer quorum work starts. The original audit
also described analogous secondary post-apply ordering; that description is
stale for the current service-stream receiver. A provider with a separate
committed read view can avoid this exposure; KV does not have that protection.

**Historical evidence (audited baseline):** Built-in `replicate` at
`kuberic-runtime/src/replicator/stateless/mod.rs:2839-2862`, with peer work
starting at `2880-2895`; secondary apply/publication at `1024-1084`.
KV visibility is in `examples/kvstore2/src/persistence.rs:344-359` and
`examples/kvstore2/src/service.rs:68-77`. Host fencing follows progress
publication at `kuberic-runtime/src/host/hosting.rs:917-953`.

**Closure condition:** Hold a barrier after visible provider mutation but before apply
returns, with a required peer unavailable. Normal primary and secondary reads
must not return the uncommitted value. Also verify commitment restores the
intended read access.

**Status: Fixed.** Local implementation/regressions:
`434cd18d70af5d907c97bc93ca7505308208635f`. Current source references below
describe that revision, not the historical audit:

- Primary `replicate` publishes ambiguity with existing counters **before**
  provider entry, keeps its abandonment guard armed, and rechecks exact
  authority after the new await
  ([stateless/mod.rs:3076-3092](https://github.com/youyuanwu/kuberic/blob/66ad81d139453047f4df2e9282d7b34c79ff7cd9/kuberic-runtime/src/replicator/stateless/mod.rs#L3076-L3092)).
  The guard credits only the already-applied floor until provider entry.
  Publication failure/drop/revocation therefore cannot manufacture an applied
  LSN just to close admission.
- Secondary ingress already publishes ambiguity **before service delivery**,
  then requires durable, verified, current-authority service ACK
  ([stateless/mod.rs:1186-1205,1255-1278](https://github.com/youyuanwu/kuberic/blob/66ad81d139453047f4df2e9282d7b34c79ff7cd9/kuberic-runtime/src/replicator/stateless/mod.rs#L1186-L1278)).
  Its older post-apply description is not a second current defect.
- Real KV provider barriers persist/install the visible mutation, release the
  map mutex, then pause before apply returns
  ([persistence.rs:403-456](../../../examples/kvstore2/src/persistence.rs#L403-L456)).
  Normal reads use the unchanged partition-admitted
  [`KvService::get`:68-77](../../../examples/kvstore2/src/service.rs#L68-L77).
  The hosted fixture uses three real replicas with quorum three, injects
  `ConnectionRefused` for the independently indispensable third member, and
  leaves the observed secondary connected. A separate in-process regression
  physically isolates both peers of a majority primary.

**Regression evidence:** `sf01_primary_visible_apply_denies_normal_reads_until_quorum_settlement`
and `sf01_secondary_visible_apply_denies_normal_reads_with_required_peer_unavailable`
hold the post-visible/pre-return barrier, prove raw `(2,1)` mutation and old
acknowledged counters, and require exact normal-read
`ReadClosed(NotPrimary)`. Gate release while quorum is unavailable does not
restore access. Real CatchUp, durable commitment/ACK and public projection
refresh restore normal reads of the exact value on all three replicas
([hosted tests:403-483](../../../examples/kvstore2/tests/read_fencing.rs#L403-L483)).
`sf01_primary_visible_apply_is_fenced_during_physical_quorum_loss` supplies
the physical disconnection/recovery observation
([in-process journey:1073-1140](../../../kuberic-level-tests/src/in_process/journeys.rs#L1073-L1140)).

The five additional hosted tests cover primary mutate-then-error, equal raw
counters plus Progress without valid settlement, actual caller drop, Abort
completion before late apply return, and secondary error/cancellation with
fresh replay/ACK recovery
([tests:492-662](../../../examples/kvstore2/tests/read_fencing.rs#L492-L662)).
After Abort the service removes its partition, so the normal read is `NotOpen`;
before Abort it is exact `ReadClosed(NotPrimary)`. Three private
`sf01_*preapply_publication*` tests prove failure/drop/revocation during
publication never enters the provider or advances counters
([read_fence_tests.rs:167-282](https://github.com/youyuanwu/kuberic/blob/66ad81d139453047f4df2e9282d7b34c79ff7cd9/kuberic-runtime/src/replicator/stateless/read_fence_tests.rs#L167-L282)).
See the [verification record](#sf-01-and-sf-11-revalidation) for commands and
the negative control that returned the uncommitted value with the old ordering.

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

**P6 status: Fixed within current partition policy.** Commit `1a792ed55ceec10b13bcac6aa70e19ce4c169327`;
actual `public_v1_postgres_partition_authority` and
`public_v1_postgres_secondary_readiness` regressions, with independent negative
and authorized ready controls. See [current owners and limits](sf-v1-p6-closure.md).
The Failure/Evidence below preserve the historical baseline, including old host
no-provider logic; current PgService remains usable without a dummy provider.
Native host stable-secondary read policy is unchanged.

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
passed in ordinary run `67074f25-598e-4f16-8fad-7c53cdcc2584` (336/336).
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
ordinary run `67074f25-598e-4f16-8fad-7c53cdcc2584`:

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
failing Revoke application callback reaches Abort; that separate SF-06
closure is recorded below.

### SF-06: Access revocation can block teardown and violate role ordering

**Contract:** Abort must not depend on graceful application work; failed Close
must reach abort containment. Actual application role changes follow the
corresponding replicator role change.

**Status:** Fixed by local implementation commits `ac16f97` and `d73db99`
(2026-10-17). No publication is implied.

**Historical failure:** Close and Abort first execute Revoke, which awaits application
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

**Implemented closure:** Revoke retains the access-only journal instruction,
without a role callback. Final catch-up keeps source replication authority;
actual demotion remains replication-before-application. Close child failures
invoke ordered Abort, skip later graceful callbacks, retain the first failure
across replay, and report failed Close after cleanup. Terminal cleanup has a
separately retained owner and initially-false witness; built-in drainage joins
listener, inbound/outbound tasks and entered stream effects. Listener join
ownership survives cancelled Close rather than being removed into its root.
See [lifecycle and task semantics](service-fabric-api-semantics.md#lifecycle-ordering)
and [Close/Abort implementation references](service-fabric-api-semantics.md#close-abort-and-tasks).

**Regression evidence:** Tests under `kuberic-runtime/src/host/` and
`kuberic-runtime/tests/public_v1_recovery.rs` exercise the actual executor.
Post-review targeted ordinary validation passed 53/53 tests, including 14
`sf06_` regressions and six real switchover/failover journeys:

- `sf06_abort_and_close_never_call_erroring_or_blocked_synthetic_none` and
  `hosted_sf06_terminal_access_denial_never_calls_synthetic_none`: both hostile
  None modes have zero callback entry; terminal partition access stays denied
  ([executor test:323-359](../../../kuberic-runtime/src/host/public_lifecycle_tests.rs#L323-L359);
  [hosted test:2548-2605](../../../kuberic-runtime/tests/public_v1_recovery.rs#L2548-L2605)).
- `sf06_swap_retains_primary_through_catchup_then_pairs_actual_roles`: both
  swap recipes retain primary replication through paused catch-up, then trace
  real replication demotion before application demotion, with no None callback.
  Existing production-planner switchover journeys retain late-write transfer,
  session continuity, successor writes and stale-writer rejection
  ([role trace:634-743](../../../kuberic-runtime/src/host/public_lifecycle_tests.rs#L634-L743);
  [real journey:1252-1409](../../../kuberic-level-tests/src/in_process/journeys.rs#L1252-L1409)).
- `sf06_close_child_failures_abort_skip_graceful_work_and_preserve_errors`,
  `sf06_failed_close_and_cancelled_abort_cannot_outrun_owned_cleanup`, and
  `hosted_public_v1_contained_close_restarts_without_replaying_interrupted_children`:
  both Close children reach abort fallback; failure remains visible/durable,
  and later graceful work cannot obstruct already-requested containment
  ([child failures:362-411](../../../kuberic-runtime/src/host/public_lifecycle_tests.rs#L362-L411);
  [pending cleanup:577-631](../../../kuberic-runtime/src/host/public_lifecycle_tests.rs#L577-L631);
  [hosted restart:3038-3187](../../../kuberic-runtime/tests/public_v1_recovery.rs#L3038-L3187)).
- `hosted_sf06_public_abort_waits_for_owned_synchronous_effect_after_waiter_drop`
  and `sf06_listener_join_survives_close_supersession_and_registry_shutdown`:
  entered synchronous effects/listener polls hold containment pending until
  released and joined, including request drop, Abort supersession and shutdown;
  real built-in task counts reach zero and the listener endpoint is released
  ([real effect:2430-2545](../../../kuberic-runtime/tests/public_v1_recovery.rs#L2430-L2545);
  [listener cut:482-556](../../../kuberic-runtime/src/host/public_lifecycle_tests.rs#L482-L556)).
- Additional `sf06_` tests cover first-error/cleanup-error replay at Revoke,
  failed-child and Cleanup cuts, actual role errors, idempotent Abort replay,
  and lost custom witnesses remaining uncontained
  ([role errors, witness and replay:746-943](../../../kuberic-runtime/src/host/public_lifecycle_tests.rs#L746-L943)).

**Source verification and limits:** Actual SF checkout revision
`3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d`, not only binding comments,
supports access/role separation, direct Abort, failed-Close abort fallback,
and final-catch-up-before-demotion. The
[pinned file:line evidence](service-fabric-api-semantics.md#verified-service-fabric-source-and-qualifications)
also records that SF's outer shutdown wrapper may return success, primary
Abort synchronously waits, a distinct Abort action list includes setup/roles,
and catch-up read/write status timing differs. Kuberic preserves failed outer
Close and conservatively denies both access modes. This closure does not prove
wait-free native Abort, arbitrary custom/application-consumer joining (SF-15),
client access enforcement (SF-02/SF-03), or primary progress correctness
(SF-D02); those require separate evidence. SF-D02's later closure is below.

### SF-07: Idle PostgreSQL removal globally drains the partition

**P6 status: Fixed for completed idle nonvoters.** Commits `584b20eb667806af11048eec59ce088f534dd956`
and `9f91a14f11041567c226b460b5fc474130265d66`;
`public_v1_postgres_selected_resources` retains PID/generation, held primary
SQL and unrelated active streaming across repeated removal and cancelled
retirement. [Current matrix](sf-v1-p6-closure.md). Original trace below remains
historical; active-voter removal is not the idle Remove contract.

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

**P6 status: Fixed selected slot/WAL ownership.** Commits `584b20e`, `1a792ed`
and `9f91a14`; `public_v1_postgres_selected_resources` executes failed incarnation
cycles beyond the queried slot budget, unpublished failure/cancellation,
cancelled Remove without retry, reconnect/receiver restart, active omission and
fresh replacement. Surviving authorized slots and live unrelated streaming are
controls. [Exact code/test/commit mapping](sf-v1-p6-closure.md). Slot absence
establishes release of that WAL pin; production disk-growth bounds and actual
power-loss recovery are not inferred. The original source trace below is retained.

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

**P6 status: Fixed at the orderly terminal observation boundary.** Commit
`1a792ed55ceec10b13bcac6aa70e19ce4c169327`;
`public_v1_postgres_orderly_progress` advances beyond admission, fences
primary/secondary with epoch/None, then queries stopped state before reopening
with no access. [Current matrix and precise limits](sf-v1-p6-closure.md).
Primary certified WAL is not secondary receive/replay or raw local flush.
This is not a guarantee for unseen SQL racing shutdown, its checkpoint,
arbitrary divergent-survivor elections, or kernel power loss. Original trace
and broader historical closure proposal below are preserved, not claimed tested.

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

**Status: Fixed.** Local implementation commits `53038e3` and `dbceb59`. Closure is limited
to completed-build snapshot-prefix proof; SF-18/19, SF-D01 and SF-20/21 remain
open and their contracts are unchanged.

**Contract:** A valid copied snapshot can become active and catch up without
manufacturing the original operation record at its snapshot boundary.

**Historical failure:** Build S through committed LSN 1 without concurrent writes.
Copy transfers no retained operation 1, but final catch-up resends operation 1
as live proof. Duplicate verification requires the missing retained record
and requests a rebuild. A provider restoring the exact record can mask this;
the valid snapshot-only provider contract does not require it.

**Historical evidence:** Copy range and completion at
`kuberic-runtime/src/replicator/stateless/mod.rs:1684-1696,2102-2136`;
boundary proof at `1384-1414`; duplicate verification at `1032-1038,802-805`.
The snapshot-only fixture is in
`kuberic-runtime/tests/public_v1_recovery.rs:443-457,489-503`.

**Closure:** Complete a nonzero build with no concurrent or subsequent write,
activate it, and perform both catch-up modes without rebuilding. Exercise
known progress and the already-known-peer `INVALID_LSN` sentinel while
retaining exact history, session, and storage validation.

**Implemented closure:** Successful source Build retains an exact completed
anchor. A separate read-only replication proof validates the target's terminal
receipt against live durable history/storage, sessions, active configuration,
endpoint and provider/cached progress, and echoes the exact proof. Original
build configuration and current active configuration are checked separately.
Source authority is rechecked after awaited proof work and before final quorum
success. The general final guard preserves the unchanged entry role epoch
across Swap's supported epoch-before-role sequence (`dbceb59`), rather than
requiring premature role replay. Prefix credit is capped at completed C and primary commitment; later
records and ordinary duplicates retain operation-exact verification. No
protected public trait changed. See `kuberic-runtime/src/replicator/stateless/mod.rs:441-472,1160-1234,1625-1764,2174-2203,3032-3057`
and `kuberic-runtime/src/host/hosting.rs:1084-1115` at `dbceb59`.

**Native comparison:** Fresh read-only source pin
`3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d` was clean. Native Build's copy
through S-1 plus replication-start S, combined final-copy/service completion
and replication apply-ACK gate, preserved session progress on promotion and
apply-ACK catch-up predicates do not require retransmitting original record
S-1. The [pinned native references and ACK qualifications](service-fabric-api-semantics.md#completed-snapshot-prefix-proof)
distinguish received/queue/service progress from Kuberic's stronger durable
provider/exact identity model. No native tests were executed and no
architecture or EOF equivalence is claimed.

**Regression evidence:** Five public factory/host tests selected by `test(sf10_)`
cover empty/nonzero quiet Build → activation → WriteQuorum and All, explicit
and already-known-peer `-1`, missing boundary records on target and source,
concurrent/later suffix, exact read-only retries, wrong/stale proof identity,
live progress/history/storage loss and source abort during delayed proof.
See `kuberic-runtime/tests/public_v1_recovery.rs:1645-1919` at `53038e3` and
[the focused command](testing.md#quiet-snapshot-boundary-proof).
Disabling only the new receipt-proof path reproduces both nonzero quiet
failures as RemoteBuildTarget / IncompatibleRetainedHistory; restored code
passes. These memory-backed cases establish sequencing, not fsync. Existing
SF-01/SF-11/SF-D02, service ACK and cancellation regressions remain separate.

## P2 Findings

### SF-11: Suffix reconciliation skips a missing committed prefix

**Historical failure:** With primary applied/committed progress (11,10), compatible peer C
at (5,5), and retained operations 6-11 available, suffix reconciliation sends
11 before repairing 6-10. C correctly rejects the gap, which is classified as
incompatible history requiring rebuild.

**Historical evidence (audited baseline):** `kuberic-runtime/src/replicator/stateless/mod.rs:1435-1443,1325-1329`;
gap rejection at `1012-1017` and rebuild conversion at `1343-1344`.

**Closure condition:** Repair the compatible committed prefix, then verify and settle the
suffix without replacement. Continue rejecting genuinely mismatching or
unavailable retained history.

**Status: Fixed (pre-existing repair, now exactly revalidated).**
`05f69bee820cc6393842d34b45ae82e2ced32981` (PR #151) changed suffix
verification to replay the missing committed prefix first. That implementation
was already present at `9c0dcf0`; SF-01's runtime patch does not change it.
`434cd18` adds the exact positive/rejection evidence:

- [`verify_peer_applied_suffix`:1472-1490](https://github.com/youyuanwu/kuberic/blob/66ad81d139453047f4df2e9282d7b34c79ff7cd9/kuberic-runtime/src/replicator/stateless/mod.rs#L1472-L1490)
  caps peer progress by primary commitment, then
  [`catch_up_peer`:1522-1559](https://github.com/youyuanwu/kuberic/blob/66ad81d139453047f4df2e9282d7b34c79ff7cd9/kuberic-runtime/src/replicator/stateless/mod.rs#L1522-L1559)
  replays through the applied tail.
- `read_fencing::sf11_committed_prefix_6_through_10_precedes_ambiguous_11_and_rejects_bad_history`
  seeds actual primary `(11,10)`, compatible peer `(5,5)` and retained 6-11
  before Open. Original service operations apply and ACK ordered
  `(LSN, watermark)` deliveries `(6,6)` through `(10,10)`, `(11,10)`, then
  `(11,11)`. Both commit 11 without logically reapplying the final watermark
  ([fixture and success:21-108](../../../kuberic-runtime/tests/support/read_fencing.rs#L21-L108)).
- A distinct peer with truthful durable `(6,5)`, stale advertised 5 and genuinely
  conflicting retained bytes at 6 gets typed remote incompatible-history
  rejection. Missing primary retained 6 gets typed local incompatible-history
  rejection with no delivery. Neither advances primary commitment beyond 10.
  A stopped peer instead leaves `ReconfigurationPending`, not a false history
  diagnosis
  ([negative cases:110-130](../../../kuberic-runtime/tests/support/read_fencing.rs#L110-L130)).

This closes the historical prefix-skipping finding only, not SF-12, SF-D02 or
other catch-up/primary-progress requirements.

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

**P6 status: Fixed silent success by explicit unsupported rejection.** Commit
`1a792ed55ceec10b13bcac6aa70e19ce4c169327`;
`public_v1_postgres_marked_catchup` rejects marked WriteQuorum while ordinary
quorum advances past the lagging required peer, without destroying primary
SQL. All and ordinary unmarked WriteQuorum controls pass; capability-false
swaps retain All. [Current matrix](sf-v1-p6-closure.md). Marked-specific quorum
is not claimed implemented; the following accepted-success trace is historical.

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
Final-review corrections are in
`30953372ab15021f40971229b0b4bec6c404c5fe`
(`fix(runtime): fence copy replay proofs and cancelled effect admission`).
Built-in ingress now delivers through `ServiceStreams` and waits for durable
service ACK (`kuberic-runtime/src/replicator/stream.rs:329-344`;
`kuberic-runtime/src/replicator/stateless/mod.rs:1105-1168,2099-2417`).
CopyComplete, including empty snapshots, is explicit; retained catch-up
requires acknowledged contiguous LSNs and an exact terminal boundary.
Post-ACK checks verify actual durable progress and exact payload/history
under current authority. Queueing or physical persistence before explicit
ACK cannot complete ingress or publish successful receiver progress.

The optional service-invoked `apply_to`/`execute` helpers
(`replicator/stream.rs:74-176`) cover apply, duplicate-byte verification,
requested commitment, cancellation and actual-effect drainage. KV/SQLite
consumers explicitly ACK/reject those results
(`examples/kvstore2/src/service.rs:216-233`;
`examples/sqlite/src/service.rs:390-403`). There is no automatic factory
consumer or double-apply receiver fallback. Primary-local apply/commit
ownership is unchanged.

**Regression evidence:** Thirteen real hosted factory/service tests in
`kuberic-runtime/tests/public_v1_recovery.rs:1480-2488` passed in final
ordinary run `67074f25-598e-4f16-8fad-7c53cdcc2584`:

- `hosted_operation_stream_copy_and_completion_wait_for_durable_ack`
- `hosted_operation_stream_live_final_requires_exact_sequence_and_copy_complete_ack`
- `hosted_operation_stream_copy_complete_requires_verified_existing_suffix`
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
`examples/sqlite/src/service.rs:463-517`).
SQLite `stream_cancellation_after_durable_apply_does_not_commit` verifies
cancellation plus stream closure against the real paused provider (`:412-458`).
All passed in the final focused 26-test selection and the final 336-test
ordinary run. Cancellation does not promise rollback; this closure
does not resolve the distinct SF-15 trait-Close task-join finding.

The correction tests reproduce the cancellation-admission race and malformed
old-sequence terminal bypass before fixing them. Deterministic same-poll
request/close races verify under-lease admission checks
(`replicator/stream.rs:379-439`). Terminal-negative tests prove no receipt
appears before a real CopyComplete ACK, including contradictory flags.
Real SQLite `sqlite_operation_stream_copy_preserves_verified_uncommitted_suffix`
uses the Default factory, TCP ingress, actual service streams and persisted
WAL/snapshot data (`examples/sqlite/tests/replacement_construction.rs:269-862`).
Truthful `(2,1)` and `(3,1)` targets retain their intact suffixes over copy,
then individually ACK retained frames without a second apply. AfterCommit
rejection may leave physical commitment ahead: acknowledged build commitment
separately bounds replay admission, replies, and terminal receipts
(`stateless/mod.rs:2140-2149,2209-2323,2334-2417`).
Exact retry with a held fresh service ACK is required to restore those proofs.

**Scope of closure:** PR #154 resolves disconnected delivery and missing
service-ACK ownership. Its replica-lifetime streams, source-local copy context,
primary-local persistence pipeline, and ACK shape/policy are not evidence of
SF compatibility; SF-18 through SF-21 track their required alignment.

### SF-15: KV Close does not join owned consumers

**P6 status: Fixed at service trait Close.** Commit `ca0c98ff8e785becf3f1358e5c7053bc3176129b`;
`sf15_trait_close_retains_cancelled_consumer_drain` uses an actual owned
secondary consumer and cancelled/retried trait Close before host exit.
`trait_close_joins_consumers_and_retains_failure_on_retry` preserves diagnostics.
[Current owner and evidence](sf-v1-p6-closure.md). Native completion remains
distinct from application commitment; the missing-join trace below is historical.

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

**P6 status: Fixed for retained gateway/grant ownership.** Commits `584b20e`
and `aebfa651c6a7865dbba139441113011a8d5fe3eb`;
`public_v1_postgres_retained_abort` separately retains actual driver and hosted
service, observes exact process/socket/listener/task reclamation without prior
Close/Drop, and injects a task join failure with other retained grants.
[Current matrix](sf-v1-p6-closure.md) separates synchronous disarm signals,
forceful exact-process cleanup and asynchronous gateway joins. The regression
deadline is not an Abort latency SLA; original missing-owner trace below remains
historical.

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

**P6 bounded status: Current-factory effective endpoint case fixed; P7 open.**
Commit `ca0c98ff8e785becf3f1358e5c7053bc3176129b`;
`public_factory_port_zero_effective_settings_are_immutable_and_round_trip`
executes the actual default factory/host/registration chain and preserves
identity/session/listener while accepting unchanged effective settings and
rejecting a different address. [Current mapping](sf-v1-p6-closure.md).
The replacement factory's pre-P6 source accepted original port zero but
rejected the effective endpoint; the removed-engine inner-guard trace below
must not be described as reproduced on current code. No original-address alias,
new settings getter/default/update capability or P7-wide closure is claimed.

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

### SF-18: Copy stream lifetime prevents SF sequential consumption

**Contract:** The SF description supports consuming the copy stream to its end
before consuming replication. Parallel consumption is supported, but is not a
requirement imposed on every service. End of stream is represented by a null
operation, distinct from a temporarily empty open stream.

**Failure:** Kuberic exposes single-take, replica-lifetime copy and replication
streams. Snapshot completion is an explicit `CopyComplete` operation; the
copy stream remains open afterward. A service following the documented SF
copy-until-EOF-then-replication pattern waits for replica shutdown instead of
starting replication. Existing concurrent consumers avoid this mismatch but
do not establish compatibility with the sequential pattern.

**Evidence:** SF
[`StateReplicator` stream descriptions, lines 100-122](https://github.com/youyuanwu/service-fabric-apps-rs/blob/615da9a886d0d1d68af63f9c3916024986309673/crates/libs/mssf-ext/src/traits.rs#L100-L122)
and
[`OperationStream`, lines 176-185](https://github.com/youyuanwu/service-fabric-apps-rs/blob/615da9a886d0d1d68af63f9c3916024986309673/crates/libs/mssf-ext/src/traits.rs#L176-L185).
Kuberic explicitly requires both consumers to stay alive and defines `None`
as terminal shutdown in `kuberic-runtime/src/replicator/traits.rs:165-185`.
`ServiceStreams` retains the copy sender until shutdown in
`kuberic-runtime/src/replicator/stream.rs:304-367`; copy completion precedes
retained replication in `replicator/stateless/mod.rs:2211-2282`.

**Required alignment:** Make snapshot-copy completion observable through the
SF stream-end contract and support sequential consumption. An internal
copy-complete marker may remain an implementation detail, not a mandatory
replacement for the service-visible end of copy. Define stream acquisition
and renewal across builds and role changes without shutting down unrelated
replication or confusing temporary emptiness with EOF.

**Closure:** Through a real hosted factory, consume and ACK copy until `None`,
then start replication and complete build/catch-up. Cover empty and nonempty
copies, a later authorized build, role transitions, terminal shutdown, and
parallel consumption. No timeout, shutdown, or mandatory concurrent consumer
may be used to manufacture normal end-of-copy.

**Native lifetime refinement (planning, not closure):** test a later build only
in a valid new native-compatible receiver lifetime. Native idle-to-active retains
the receiver, repeated getters share its queue without rewinding, and duplicate
Build on an already-known replica is rejected; normal EOF does not imply arbitrary
in-place stream renewal. Ordinary EOF and configured explicit EOS-ACK mode are
distinct. See the [source-backed receiver design](sf-v1-semantic-alignment-plan.md#3-receiver-lifetime-streams-and-target-context).

### SF-19: Copy context comes from the primary instead of the secondary

**Contract:** A newly opened idle secondary supplies provider-defined context
to the primary. The primary passes that target context to `get_copy_state` to
select the required state. Providers that need no context may use an empty
exchange; that does not remove the secondary-origin requirement for providers
that do need it.

**Failure:** The built-in primary invokes its own provider's
`get_copy_context`, then passes that result to its own `get_copy_state`.
No target-to-source provider-context exchange occurs. Providers that rely on
the target's existing state therefore receive context from the wrong replica.

**Evidence:** SF
[`StateProvider`, lines 35-73](https://github.com/youyuanwu/service-fabric-apps-rs/blob/615da9a886d0d1d68af63f9c3916024986309673/crates/libs/mssf-ext/src/traits.rs#L35-L73).
Kuberic source-local calls are in
`kuberic-runtime/src/replicator/stateless/mod.rs:1673-1677`; the current
limitation is documented in `kuberic-runtime/src/traits.rs:64-95`.

**Required alignment:** Obtain context from the exact admitted idle secondary
and transport it to the source provider. Preserve build/session/storage/history
binding, cancellation, and the frozen copy boundary. Do not replace the exchange
with source context or a silent empty-context fallback.

**Closure:** Use distinct source and target providers whose context bytes are
observably different. Prove that source `get_copy_state` receives the target's
context and builds the correct state. Cover empty context, streamed context,
context failure/cancellation, and replacement of the target process/session
without accepting obsolete context.

### SF-20: Primary replication lacks the SF early-LSN and local-commit pipeline

**Contract:** SF returns the assigned LSN immediately, separately from eventual
quorum completion. This enables the service to prepare its local write while
replication is pending and commit when completion permits. Application-local
persistence remains a service responsibility rather than a prerequisite
engine-driven `DurableState` pipeline.

**Failure:** Kuberic's async `replicate` exposes the LSN only after completion.
The built-in engine additionally applies the primary's operation and commits
its store around quorum collection. A service using SF's early-LSN/local-write
pipeline cannot preserve that behavior by implementing only the corresponding
service/provider contract.

**Evidence:** SF
[`StateReplicator::replicate`, lines 79-98](https://github.com/youyuanwu/service-fabric-apps-rs/blob/615da9a886d0d1d68af63f9c3916024986309673/crates/libs/mssf-ext/src/traits.rs#L79-L98).
Kuberic's completion-only result and primary persistence ownership are explicit
in `kuberic-runtime/src/replicator/traits.rs:140-163`. Engine-driven apply and
commit are in `replicator/stateless/mod.rs:2997-3013,3073-3098`.
The SF interface does not prescribe Kuberic's additional `DurableState`
integration; the local ownership contrast is also recorded in
[Stateful API and Trait Compatibility](service-fabric-alignment.md#stateful-api-and-trait-compatibility).

**Required alignment:** Expose operation assignment separately from eventual
completion and support service-owned primary preparation/commit ordering.
Reconcile the existing built-in persistence dependency rather than adding an
early number while retaining a conflicting automatic local-write owner.
Preserve exact authority, contiguous history, PC/CC quorum predicates, and
honest ambiguous outcomes. `Replicator`, `PrimaryReplicator`, and
`StateProvider` remain protected interfaces; this finding does not authorize
changing them.

**Closure:** Hold secondary ACKs and obtain the assigned LSN while completion
remains pending. Let the real service prepare its local write using that LSN,
then release quorum and verify the intended local commit without double
application. Cover concurrent assignments, quorum failure, cancellation, and
epoch change without claiming rollback of already durable effects.

### SF-21: Acknowledgement requires Kuberic progress and always-explicit ACK

**Contract:** SF operation ACK reports successful local application without an
applied/committed progress argument. Persisted services require explicit ACK.
Volatile services can use implicit acknowledgement on receipt unless configured
to require service ACK. Successful replication still requires the applicable
replica quorum.

**Failure:** `StreamOperation::acknowledge` requires a
`DurableApplicationProgress` payload and every operation requires explicit
service ACK. There is no volatile implicit-ACK policy. This is a stricter
Kuberic-specific contract, not evidence of unsafe persisted ACKs, but it does
not expose the SF application-facing ACK semantics selected for alignment.

**Evidence:** SF
[`Operation::acknowledge`, lines 161-173](https://github.com/youyuanwu/service-fabric-apps-rs/blob/615da9a886d0d1d68af63f9c3916024986309673/crates/libs/mssf-ext/src/traits.rs#L161-L173).
Kuberic's explicit-only requirement and ACK payload are in
`kuberic-runtime/src/replicator/stream.rs:27-34,178-194`; receiver delivery
waits on this progress-bearing completion in
`replicator/stateless/mod.rs:1110-1140`.

**Required alignment:** Support the SF service-facing ACK contract without
requiring its caller to supply Kuberic-specific progress, and distinguish
persisted explicit ACK from the supported volatile implicit/required-ACK
policies. Retain necessary progress/history validation internally; do not
weaken persisted durability or credit a missing persisted-service ACK merely
to reproduce a method signature.

**Closure:** A persisted service can apply and explicitly ACK without supplying
progress in the ACK call; withholding or dropping that ACK prevents successful
completion. A volatile service configured for implicit ACK can complete on
receipt, while required-service-ACK mode stays pending until ACK. Verify
quorum, authority changes, rejection/failure handling, and progress reporting
for each supported policy without misrepresenting volatile ACK as durable
persistence.

## Documentation and Contract Reconciliation

### SF-D01: Catch-up capability is a tail, not a retained floor

**Mismatch:** The contract describes the earliest retained catch-up progress,
but the built-in implementation returns `state.committed_lsn`. A log retaining
operations 1-10 can report 10 even though an intermediate peer can catch up.
The actual catch-up range uses peer current progress, not this capability.

**Evidence:** `kuberic-runtime/src/replicator/mod.rs:173-178` and
`kuberic-runtime/src/replicator/stateless/mod.rs:2532-2541,1384-1392`.

The lifecycle/control-plane follow-up confirms the same mismatch:
[upstream lines 110-116](https://github.com/Azure/service-fabric-rs/blob/7c1ea4e5f9ab5fbe4cfa0b3e9bfaf37169d2b43f/crates/libs/core/src/runtime/stateful_traits.rs#L110-L116)
describe the beginning of the log, while Kuberic at `1496fd8` returns
`state.committed_lsn` in
`kuberic-runtime/src/replicator/stateless/mod.rs:2690-2701`.
SF-D01 remains open.

**Closure:** Decide and document the scalar's meaning, including empty,
snapshot-only, and pruned history and `INVALID_LSN`. Align behavior or record
an intentional conservative deviation. Returning a conservative tail is not,
by itself, evidence of unsafe quorum acknowledgement or data corruption.

### SF-D02: Primary progress promise exceeds the built-in query result

**Status: Fixed.** Behavioral reconciliation at `697f281`, with failed-settlement
query coverage completed at `ae499ff`, closes this documentation/behavior
mismatch only. At that closure, the 21 implementation findings were 6 fixed / 15 open.

**Original mismatch:** Kuberic's `Replicator::current_progress` contract promises locally
durable quorum-committed progress on a primary, but the built-in query returned
`state.applied_lsn` without a role distinction. Primary replication publishes
that applied tail before collecting quorum acknowledgements or committing it.
The result can therefore exceed the primary's quorum-committed boundary.

**Evidence:** At the lifecycle/control-plane baseline `1496fd8`, the promise is
in `kuberic-runtime/src/replicator/traits.rs:45-51`, the query is in
`replicator/stateless/mod.rs:2682-2688`, and primary apply/publication precedes
quorum collection at `:2997-3023`, with commitment at `:3073-3098`.
The upstream
[current-progress description, lines 105-108](https://github.com/Azure/service-fabric-rs/blob/7c1ea4e5f9ab5fbe4cfa0b3e9bfaf37169d2b43f/crates/libs/core/src/runtime/stateful_traits.rs#L105-L108)
describes a secondary's end-of-log for selection and catch-up. It does not
establish Kuberic's additional primary quorum-committed guarantee.

**Qualification:** The original entry was a source-confirmed local contract/behavior mismatch,
not a dynamically reproduced unsafe election, read, or data-loss finding.
Durably accepted secondary progress and applied progress used only during
access-closed recovery must not be mislabeled as unsafe primary commitment.
SF-01 separately tracks visible application state before the read fence.

**Implemented closure:** Internal Primary now selects the acknowledged committed
cache, including access-closed writes and recovery. Opened role None returns the
validated retained applied tail for election only; secondary retains contiguous
durably acknowledged acceptance. A closed access gate never changes a Primary
query into an applied-tail query
([query:2765-2775](https://github.com/youyuanwu/kuberic/blob/66ad81d139453047f4df2e9282d7b34c79ff7cd9/kuberic-runtime/src/replicator/stateless/mod.rs#L2765-L2775);
[contract:45-58](../../../kuberic-runtime/src/replicator/traits.rs#L45-L58)).
The independent host report still carries accepted/applied and committed
boundaries; suffix recovery and committed-watermark propagation remain separate
from access admission. Typed recovery's historical equal-field scalar snapshot
is not proof that its retained tail was committed. See the
[exact role table, consumer distinctions and pinned local SF source](service-fabric-api-semantics.md#role-specific-progress-and-recovery).

**Executed evidence:** The actual public scalar and journalled Progress outcome
remain 0 after local apply while the real peer service ACK is held, and remain 0
after quorum while local commit is held. Successful local commitment advances
the scalar to 1. Joined cancellation and completed epoch barriers retain scalar
0 with report `(0,1)` and access denied. Secondary scalar stays 0 before service
ACK and becomes 1 after ACK `(1,0)`, without asserting committed/readable data;
gaps are rejected
([ACK/cancel/epoch:17-134](../../../kuberic-runtime/tests/support/read_fencing.rs#L17-L134);
[secondary:138-165](../../../kuberic-runtime/tests/support/read_fencing.rs#L138-L165)).
Temporarily restoring only the old query made the held-ACK assertion fail
with actual 1 versus expected 0; the fix was restored before passing checks.

The SF-11 matrix verifies opened None scalar 11, internal Primary scalar 10,
independent report `(10,11)`, actual secondary-to-primary promotion, exact
committed-prefix-before-suffix replay and final scalar 11. Divergent/missing
history and unavailable peers keep scalar 10
([matrix:168-341](../../../kuberic-runtime/tests/support/read_fencing.rs#L168-L341)).
The typed Preserve data-loss recipe uses a real retained `(11,10)` suffix,
observes committed scalar `(10,10)` as Recovery while independent report
remains `(10,11)` with access denied, then settles/adopts history without
losing payload
([recovery:4771-4782,4990-5054](../../../kuberic-runtime/tests/public_v1_recovery.rs#L4771-L5054)).
Commit failure, Closed errors, and drained epoch reconciliation after a real
committed effect are separately asserted
([close/epoch:3449-3536](../../../kuberic-runtime/tests/public_v1_recovery.rs#L3449-L3536);
[commit error:4353-4409](../../../kuberic-runtime/tests/public_v1_recovery.rs#L4353-L4409)).
Memory-backed cases establish sequencing, not fsync; provider-backed SF-01
regressions remain unchanged. No unsafe election or data-loss reproduction
is claimed.

### Known-Replica Configuration Sentinel Clarification

The upstream
[configuration-input description, lines 161-165](https://github.com/Azure/service-fabric-rs/blob/7c1ea4e5f9ab5fbe4cfa0b3e9bfaf37169d2b43f/crates/libs/core/src/runtime/stateful_traits.rs#L161-L165)
allows `-1` when the replicator already knows a configured or built replica.
This can omit a value already known to the replicator; it does not mean that
the replica has empty history. Kuberic's earlier wording described only unknown
or inapplicable progress and omitted that input-specific meaning.

The sentinel and descriptor Rustdoc now distinguish omitted known-replica
configuration input from unknown/inapplicable query results. Zero remains the
valid empty-history boundary; `INVALID_LSN` is not completed progress and
cannot authorize reuse across different process sessions or storage
incarnations. Idle build targets continue to use the sentinel.

This wording clarification does not prove that every built-in path resolves
omitted progress correctly, and no new runtime defect or conformance closure
is claimed. SF-10 retains its known-peer `INVALID_LSN` closure case.

## Audit Boundaries and Intentional Differences

The audit did not count async signatures, omission of COM cancellation tokens,
explicit primary epoch replay, or exact Public V1 identity/session/storage
authority as violations merely because they differ from SF.

The 2026-10-17 follow-up explicitly selects SF alignment for copy stream
completion, secondary-origin copy context, early-LSN/service-owned primary
completion, and service ACK policy. Those differences are now open SF-18
through SF-21, not intentional exceptions covered by this boundary statement.

Other distinctions remain explicit rather than being counted as extra findings:

- Built-in applied progress during access-closed recovery is not automatically
  unsafe primary committed progress. SF-D02 tracks the primary-query promise
  separately from SF-01's visibility interval.
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

Final state, run `67074f25-598e-4f16-8fad-7c53cdcc2584`:

```sh
cargo nextest run -p kuberic-runtime -p kuberic-controller \
    -p kuberic-level-tests -p kvstore2 -p sqlite-replicated \
    --all-features --profile ordinary --test-threads 1
```

**336 passed, 4 skipped**, 580.588 seconds, including all named regressions
above. Final focused contract/provider selection: **26 passed**, 63.106s
(`0ecd1799-4e5f-45b3-ac45-759c6837a712`).
Earlier Phase 1 runs passed 331 ordinary and 52 recovery/provider/stream
tests; these precede the final-review corrections and are not substituted
for the final-state runs.
Workspace format/check/all-features clippy with warnings denied, doctests
(2), source API inventory (4), privacy (1), and executable built-in
conformance (1; `just nextest-build-only`, run
`f0fc49cc-47f4-4577-9a10-8e5fe8305f0d`, 71.074s) passed.

The approved `just nextest-postgres-smoke` passed **8 tests** (run
`00cd4814-b216-4cdf-b3c7-3437d1c27aac`, 116.480 seconds) with
`KUBERIC_POSTGRES_PUBLIC_V1_TMPDIR=/tmp`. An initial long checkout-local socket
root failed all eight before readiness; the standard short root resolved
that environment limit. Earlier ordinary iterations exposed retry, retained
boundary, and primary-report regressions. Final Astra review additionally
found the admission race, malformed terminal bypass, and real SQLite suffix
regression; all were corrected and reviewed before the definitive passing
runs. These results do not close unrelated PostgreSQL findings.

Exact inventory validation measured **382 = 336 ordinary + 41 PostgreSQL +
3 KinD live + 2 build-only**; all tier lists and configured partition unions
passed (ordinary slices `84,84,84,84`; PostgreSQL hashes `11,13,8,9`).
Full PostgreSQL, live KinD, and archive rebuild were not executed.
SF-14's original resolution was verified locally without publication. It was
subsequently published in PR #154.

### SF Trait-Semantics Follow-Up

SF-18 through SF-21 were documented from the pinned upstream descriptions and
Kuberic source at the follow-up baseline. They are open requirements with
specific closure criteria, not implemented changes or dynamically executed
failure reproductions. Existing SF-14 regression results establish delivery
and ACK ownership only; they do not satisfy these additional alignment gates.

The lifecycle/control-plane comparison at `1496fd8` and upstream `7c1ea4e`
confirms that the intended role ordering, idle-build admission, PC/CC quorums,
required-successor catch-up, and double-catch-up comments largely agree.
That documentation-only comparison did not close SF-06 or SF-D01. SF-D02 and the known-replica sentinel
clarification come from source inspection, not new behavioral reproductions.
This documentation follow-up changes neither runtime behavior nor protected
trait signatures.

### SF-01 and SF-11 Revalidation

2026-10-17, implementation/regression revision
`434cd18d70af5d907c97bc93ca7505308208635f`, based on `9c0dcf0`.
Only SF-01 runtime admission and SF-11's evidence-backed status were changed
by this follow-up. Secondary service-stream fencing was already present.
The SF source checkout was read-only, clean and pinned to
`3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d`; exact production and test-service
paths and their limitations are recorded in
[source qualifications](service-fabric-api-semantics.md#verified-service-fabric-source-and-qualifications).

Mandatory implementation review passed. Fresh reviewer selections:

```sh
cargo nextest run -p kuberic-runtime -p kvstore2 -p kuberic-level-tests \
  --all-features --profile ordinary --test-threads 1 \
  -E 'test(sf01_) or test(sf11_committed_prefix_) or test(sf06_) or binary_id(=kuberic-runtime::lifecycle_capability_boundaries) or binary_id(=kuberic-runtime::public_api_inventory)'
```

**33/33 passed**, run `a3573a17-05b4-4978-b77a-804f57f56b13`, including all
12 new regressions, existing SF-06 roots/listeners/terminal ownership and seven
protected-interface/source-API guards. A separate adjacent selection passed
**19/19**, run `7777c855-710d-4df6-9961-ae66616ac937`: primary revocation,
Close/drop, commit failure, prior suffix matrix, hosted stream ACK/cancellation,
real KV restart/handoff and physical quorum-loss recovery.

The primary negative control temporarily restored old post-apply-only fencing:

```sh
cargo nextest run -p kvstore2 --all-features --profile ordinary --test-threads 1 \
  -E 'test(=sf01_primary_visible_apply_denies_normal_reads_until_quorum_settlement)'
```

It failed as intended with actual normal-read `Ok(Some("uncommitted"))`
while apply was held (run `7edbf220-2c3a-4f28-a8e6-cce69bda6650`).
The fix was restored before subsequent passing verification and commit.
This is a visibility reproduction, not a dynamically reproduced data-loss claim.

Targeted all-target/all-feature check and warnings-denied clippy for runtime,
KV and level-tests, workspace formatting, API inventories/privacy guard and
diff checks passed. `just nextest-validate` measured **408 = 362 ordinary +
41 PostgreSQL + 3 KinD + 2 build-only**, with ordinary slices `91,91,90,90`
and PostgreSQL hashes `11,13,8,9`; exact-once tier/partition accounting passed.

Routine `just nextest-postgres-smoke` passed **8/8**, run
`5ea307e5-dc98-4d0c-b502-7f642ee22951` (87.875 seconds), with the short local
socket root `KUBERIC_POSTGRES_PUBLIC_V1_TMPDIR=/data/code/kuberic/.pg`.
The smoke recipe's non-testing binary build emitted existing feature-gated
unused-code warnings; the all-features warnings-denied clippy selection passed.
No full PostgreSQL suite or live KinD run was performed. These checks do not
close unrelated provider/application findings or SF-D02.

### SF-D02 Query Reconciliation

2026-10-17, implementation `697f281` and required-review coverage fix `ae499ff`,
based on merged SF-01 baseline `f569499`. The local SF reference was verified
clean at `3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d` before/after research.
The [role/native-source section](service-fabric-api-semantics.md#role-specific-progress-and-recovery)
distinguishes normal quorum-derived native primary queue commitment, received
persisted-secondary progress, inherited recovery/promotion prefixes, transactional
wrapper delegation and separately calculated access. Kuberic's stronger local
durability promise is not inferred from the pinned Rust secondary-log comment.

Focused semantic/SF-01/SF-11/error/epoch/recovery/API union passed **23/23**,
run `f8406317-9ffe-4295-8586-11c51094e456`. The old-query negative control
failed at the held-ACK public scalar, actual 1 versus expected 0, run
`021cf94d-3e78-46a9-9306-113b94a9637e`; restored code passed.
The documented [focused selector](testing.md#role-specific-scalar-progress)
reproduces the progress-specific coverage.

```sh
cargo nextest run --workspace --all-features --profile ordinary --test-threads 2
just nextest-validate
just nextest-postgres-smoke
just nextest-archive
just nextest-validate-archive
```

Complete ordinary tier: **364/364 passed**, run
`a85cacbc-b316-4848-bb0b-d5890307d974` (419.681 seconds).
Both inventory validators measured **410 = 364 ordinary + 41 PostgreSQL +
3 KinD + 2 build-only**, ordinary slices `91,91,91,91`, PostgreSQL hashes
`11,13,8,9`, exact-once accounting passed.
Archive build includes passing rustdoc/source-public/private-capability guards
and build-only conformance. A fresh-source archive consumer with no package
target directories passed **18/18** semantic/SF-01/SF-11/epoch/recovery cases,
run `ed8b63e6-2757-4308-9778-ce5d4b60a990` (53.052 seconds).

Routine PostgreSQL smoke passed **8/8**, run
`0544fa79-3bc3-4105-bf3c-567739902b6e` (113.542 seconds), using
`KUBERIC_POSTGRES_PUBLIC_V1_TMPDIR=/data/code/kuberic/.pg`.
An initial archive/API attempt exhausted disk; the concurrent initial smoke
was 7 passed/1 failed with explicit WAL ENOSPC and catch-up timeout.
After removing inactive regeneratable incremental build cache, complete
archive/API and eight-test smoke retries passed without source changes.
The smoke binary build emitted existing feature-gated unused-code warnings;
all-target/all-feature runtime clippy with warnings denied, formatting,
runtime doctests and diff checks passed.
No full PostgreSQL or live KinD suite was run. At that revalidation, SF-D01 and
15 implementation findings remained open; SF-01/SF-11 regression coverage was green.
