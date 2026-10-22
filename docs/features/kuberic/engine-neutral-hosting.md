# Engine-neutral hosting

This is the bounded common-host responsibility port on top of merged #198
(`2f97d86605ce69f3812c7d5ba1dd9c6d606ca8d7`), implemented through
`91a2a93c3c7cf9477adb3a0d27e674b99f741a8a`. Native reference:
Service Fabric [`3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d`][native].
It is source-guided Rust/Kubernetes behavior, not native executable equivalence,
COM/wire compatibility, wholesale RA/FM/Hosting2 or production certification.

## Owners and independent facts

Both the built-in engine and independent PostgreSQL allocation attach the same
private [creation-bound coordinator][coordinator]. It owns serialized exact
grant install/revoke, retained response-independent actions, compensation,
receiver history and actual drain results. The [built-in effects][production]
own queues/carrier/provider joins; the [independent adapter][companion] invokes
the engine's own ingress and shutdown. An independent engine does not borrow a
built-in allocation, private attachment or native diagnostic owner.

The common [operation registry][operations], [instruction executor][program],
[admission projection][admission] and [selected access owner][access] retain
request authority, achieved receipts and lifetime policy. There is no parallel
readiness ledger or second topology scheduler.

| Report/control fact | Meaning | Does not prove |
|---|---|---|
| `configuration.voting` | Successful host installation for exact current history/storage/process | A secondary executed a primary-only configuration callback; remote admission; serving access |
| `remote_builds` | Exact journal-derived Accepted / InBuild / Ready / InDrop / Dropped | Observed engine idle membership or a fresh live copy grant |
| `engine_idle = None` | Optional query unavailable or discarded as incoherent | Observed empty membership |
| `engine_idle = Some(... idle_replicas: [])` | Actual empty query, coherently bound to configuration/history/storage/process/creation | Copy readiness or application durability |
| Successful Build completion | Exact retained operation/attempt succeeded | Controller already accepted it, revoked ingress or admitted a voter |
| Positive containment | Successful actual owned drainage | Successful child Close or erased child error |

The [report types and validation][report] keep these facts separate. Torn,
late, duplicate or voter-overlap samples suppress only the optional sample,
not truthful installed host authority. Ready requires exact successful retained
completion; retirement, supersession and terminal authority dominate old success
([projection][remote]). No projection installs a grant.

Native V1 manager idle membership can exist while copy is still running.
PostgreSQL's native `built_idle` observation publishes only completed copies.
Both meanings are preserved. Neither is a universal readiness certificate.
Native FUP likewise owns InBuild/Ready/InDrop separately from engine queries;
native internal-query support is optional.

Native RAP's Build/Remove request view describes the remote replica as idle
even when other control-plane descriptions carry a voting role. Local explicit
Build/Remove intents likewise do not implicitly admit a voter.

## Pinned message → action → state mapping

Native paths below are under `src/prod/src/Reliability/Failover/ra/` at the pin.
Ranges are inspected source evidence, not executed native tests.

| Native message/owner | Source behavior | Local owner and intentional adaptation |
|---|---|---|
| `Message.h:20–51`, `ProxyRequestMessageBody.h`, `ProxyReplyMessageBody.h`; RAP dispatch `ReconfigurationAgentProxy.cpp:180–280` | Named Open/Close/Reopen/UpdateConfiguration/Build/Remove/epoch/status/cancel/query; request carries runtime, FU/local/remote/service/flags | Public control programs carry exact identity/process/history/storage/epoch/configuration/attempt. Durable acceptance and typed results replace native IPC/no-reply retry; not native wire serialization. |
| `FailoverUnitProxyContext.h:58–149,194–252`; `FailoverUnitProxy.cpp:133–250`; action compatibility table | Revalidate epoch/instance/state; incompatible actions wait/retry; close business before cancellation | Registry plus transactional admission rejects stale/changed identities, serializes incompatible authority and permits compatible unrelated Builds. Exact active duplicates attach; completed duplicates replay immutable results. |
| `ComProxyStatefulService.OpenAsyncOperation.cpp:17–39`; `FailoverUnitProxy.OpenReplicaAsyncOperation.cpp:112–162`; `ComProxyReplicator.h:17–35` | Service Open returns coherent required interfaces; internal query and specific-quorum interfaces optional | One-shot factory reservation and coherent interface registration; explicit supplemental ownership, optional diagnostics and existing specific-quorum capability. Protected replication/provider contracts unchanged. |
| `ProxyActionsList.cpp:34–193` | Ordered Open, promotion/demotion, catch-up/current configuration and role/access actions | Frozen instruction programs and controller-composed stages; successful prefixes are distinct from pending/failed callbacks. Paired roles, epoch, exact configuration and fresh promotion catch-up qualify access. |
| `ReconfigurationAgentProxy.cpp:761–974`; `FailoverUnitProxy.cpp:636–920` | State-sensitive UpdateConfiguration selection; pending versus completed configuration; no older joint catch-up after completed current stage | Common gate checks all actually selected configuration stages, including Swap's starting set. It applies to primary engine configuration, not a secondary's host-only transport installation. |
| Same native UpdateConfiguration selector | Current-stage peer updates differ from stale Catchup-flag messages | Fresh current-only synchronization supports planned handoff, recovery and renewed/built peer admission at frozen authority. Renewal does not reopen PC/CC after finalizing a retained subset. Selected catch-up cancellation still requires a newer epoch. |
| `FailoverUnitProxy.ReplicatorBuildIdleReplicaAsyncOperation.cpp:15–217,256–357`; `FailoverUnitProxy.cpp:960–1064` | FUP tracks exact remote operations/tombstones; accepts success only while still InBuild; Remove/cancel invalidates late completion | Existing exact Build/retirement journal projects remote state; source Build needs achieved paired Primary and installed authority. Exact Remove and supersession fence callbacks. Failed Build does not automatically restart. |
| `FailoverUnitProxy.cpp:1483–1664`; user API invoker `:50–64` | Cancellation can race Begin; drain waits owned work | Retained Rust roots/control tasks and compensation joins outlive RPC observers. Cancellation invalidates access and aborts **and joins** roots; engine descendants require their declared ownership/drain boundary. |
| `ReconfigurationAgentProxy.cpp:1386–1456`; Close child operation `:78–111` | Native outer Close may Abort and send success after child failure | Deliberately different local result: abort/join still runs, but child failure remains a failed public completion even after positive containment. Failed/lost drain never becomes positive containment. |

This maps the relevant RAP/FUP/common-proxy semantics, not full RA
reconfiguration stages, FM placement, Hosting2 process management, native
compatibility tables, settings or automatic recovery policy.

## Acceptance, retry and access

Exact input digest, operation ID/revision, resource, process session, storage,
history, epoch, configuration and Build attempt remain fenced. Higher numeric
revision cannot revive lower epoch or selected cancelled catch-up. Ordinary
duplicate failure is not universal success.

The journal has a nonterminal `RetryPending` boundary for a **proven, selected
returned-callback failure**, with immutable per-attempt waiter results and exact
successful-prefix restoration into a new fenced access generation. Earlier
waiters cannot erase results or revoke later generations. Rebuild obligations,
unknown/host/receipt failures, interruption, data-loss ambiguity, Build and
destructive terminals do not borrow that proof.

**Current production engines declare no returned-failure repeatability.**
Positive failed-prefix resumption is qualified with explicit private fixtures;
actual V1/PostgreSQL undeclared application failures remain containment-required.
`CallbackOwnership::NativeObserver` is not repeatability or successful drainage.
It applies only to the engine's selected callbacks, never independent
application Role/Open or the application's Access refresh. The conservative
default is `ObjectOwnedOnInterruption`.

Promotion receipt restoration never imports unrelated historical catch-up.
Secondary preparation, idle Build and one successful role child never grant
serving access. Native epoch/progress observations retain their real meaning;
descriptor readiness is not live health or application commitment.

## Controller copy, renewal and retained lifetime

Production dispatch freezes exact target authority, awaits its installation,
then calls source Build. Source completion is distinct from controller
acceptance: exact revoke precedes separate joint/current voter admission.
Revoke withdraws only matching ingress, not receiver ownership, existing
replication or voting configuration.

Renewal keeps its command/access fence while admitting only the exact
ConfigurationPending replacement copy needed to finish that renewal. Exact
predecessor containment, replayed replacement role, source/target endpoints,
process/creation/storage/history/epoch remain required. This is not a general
fenced-operation bypass. After lost replies/controller recreation, the
[renewal owner][renewal] reconstructs only exact compare-revoke from retained
frozen Build input plus successful completion before further configuration.
Historical completion never recreates ingress.

Close/Abort stop admission and revoke access before actual retained joining.
A lost response does not discard installation, cleanup or compensation.
Returned drain errors and lost native task joins are sticky across later
observers. Protected Close/Abort must independently wake blocked companion
actions before serialized drain, avoiding installer/drain dependency cycles.

## Compatibility and qualification

Host metadata replacement schema is **14**; schema 13 is deliberately rejected,
not silently upgraded or defaulted into new authority. Application/provider
data formats are unchanged. The unstable Public V1 report/wire adds separate
optional `engine_idle` and exact `remote_builds`; the host configuration wrapper
no longer embeds idle membership. No rolling old/new control-wire guarantee is
claimed.

Executed Rust qualification includes:

- [Actual V1 controller journeys][v1-tests]: held real nonempty copy, native idle
  before completion, exact remote state/grants, separate voting; existing real
  failover/switchover/recreation/renewal/isolation and retained observer paths.
- [Actual independent PostgreSQL controller journeys][pg-tests]: backup/lineage/
  receive/replay, old/new SQL fencing, exact retries/revocation/invalidation;
  hosted public terminal success, contained child failure and lost real join.
- Both engines' undeclared callback failures remain fail-closed through exact
  production redispatch. Direct negative fault cuts are setup, not substitutes
  for controller dispatch.
- Host/store/wire/coordinator and synthetic companion negatives separately
  qualify stale/torn/missing observations, immutable attempts, failed receipts,
  cancellation, retained joins and exact scope. Seeded records are identified
  as fixtures, not native executions.

Use the [explicit validation commands](testing.md#engine-neutral-hosting)
for behavior and public API/privacy/Rustdoc/production package guards. Filtered
nextest discovery is not evidence that source/ignored guards ran.

No native binary differential, arbitrary failed-Build recovery, unpublished
MySQL qualification, broad security/settings expansion, kernel power-loss
or production certification is claimed.

[native]: https://github.com/microsoft/service-fabric/tree/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Failover/ra
[coordinator]: ../../../kuberic-runtime/src/replicator/coordinator.rs#L1
[production]: ../../../kuberic-runtime/src/replicator/v1/service_adapter/production.rs#L223
[companion]: ../../../kuberic-runtime/src/replicator/custom.rs#L1
[operations]: ../../../kuberic-runtime/src/host/operation.rs#L1498
[program]: ../../../kuberic-runtime/src/host/public_lifecycle_program.rs#L10
[admission]: ../../../kuberic-runtime/src/host/public_action_state.rs#L110
[access]: ../../../kuberic-runtime/src/host/selected_access.rs#L580
[remote]: ../../../kuberic-runtime/src/host/state.rs#L270
[report]: ../../../kuberic-runtime/src/protocol/public_operations.rs#L2476
[renewal]: ../../../kuberic-controller/src/cluster_api.rs#L2374
[v1-tests]: ../../../kuberic-level-tests/src/in_process/journeys.rs#L1
[pg-tests]: ../../../examples/postgres/src/public_controller_tests.rs#L186
