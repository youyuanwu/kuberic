# SF V1 P6: application and custom-replicator closure

P6 qualifies the selected Public V1 PostgreSQL path, KV service Close, and the
current default factory's effective endpoint. Historical counterexamples in the
[audit](service-fabric-contract-audit.md) are preserved; removal of the old
engine alone did not close these findings. P7's broader capability/settings
ledger and P8 qualification remain separate.

## Finding-to-code-to-regression matrix

The commits below are local implementation references, not publication claims.
PostgreSQL cases are exported as `tests::<name>` in the postgres-replicated
library binary and included in the existing smoke catalog.

| Finding / disposition | Current selected owner | Executed regression and implementation |
|---|---|---|
| SF-03 — fixed within partition policy | [Live status sampling and access reconciliation](../../../examples/postgres/src/public_v1.rs#L626-L743); [captured receiver boundary](../../../examples/postgres/src/public_v1.rs#L1873) | `public_v1_postgres_partition_authority` uses actual PgService/factory/host, no StateProvider, authorized SQL then status-only revocation of existing/new access with unchanged lifecycle. `public_v1_postgres_secondary_readiness` independently holds actual receive ahead of replay, denies early SQL, releases replay for permitted read, then revokes authority. Commit `1a792ed55ceec10b13bcac6aa70e19ce4c169327`. |
| SF-07 — fixed for completed idle nonvoters | [Selected Remove](../../../examples/postgres/src/public_v1.rs#L1919-L1954) | `public_v1_postgres_selected_resources`: Remove twice retains primary PID/generation, held SQL, installed peer and continuing synchronous streaming. Active-voter Remove rejects; authority retirement uses None/configuration instead. Commits `584b20eb667806af11048eec59ce088f534dd956`, `9f91a14f11041567c226b460b5fc474130265d66`. |
| SF-08 — fixed selected slot/WAL ownership | [Shared explicit-owner reconciliation](../../../examples/postgres/src/native.rs#L198-L258), [cancellation-safe retirement obligation](../../../examples/postgres/src/public_v1.rs#L784-L792) | `public_v1_postgres_selected_resources`: SQL inspection of slots across failed sequential incarnations exceeding the queried configured budget, cancellation/failure after reservation before grant publication, cancelled Remove without retry, active omission and fresh target replacement. Authorized peer slot survives reconnect/receiver restart and unrelated SQL/streaming stays live. Existing basebackup cancellation/retry and stale-successor cases remain controls. Commits `584b20e`, `1a792ed`, `9f91a14`. |
| SF-09 — fixed orderly terminal observation | [Pre-stop snapshot](../../../examples/postgres/src/public_v1.rs#L567-L617), [stopped physical-role progress](../../../examples/postgres/src/public_v1.rs#L1507-L1534) | `public_v1_postgres_orderly_progress`: actual primary certified and secondary receive/replay positions advance beyond persisted admission, then epoch/None fences preserve them before any reopening, with no SQL access. Commit `1a792ed55ceec10b13bcac6aa70e19ce4c169327`. |
| SF-13 — fixed silent success; marked WriteQuorum explicitly unsupported | [Marked-input rejection and cancellable read-only wait](../../../examples/postgres/src/public_v1.rs#L1476-L1505) | `public_v1_postgres_marked_catchup`: pause required peer while another certifies new WAL, require explicit unsupported error without stopping primary; All and ordinary unmarked WriteQuorum still succeed. Pending All cannot monopolize lifecycle ownership and prevent status-only revocation. Commit `1a792ed55ceec10b13bcac6aa70e19ce4c169327`. |
| SF-16 — fixed retained gateway containment | [Retained public owner](../../../examples/postgres/src/public_v1.rs#L412-L522), [driver-rooted gateway](../../../examples/postgres/src/public_transport.rs#L80-L163) | `public_v1_postgres_retained_abort`: retain actual driver and separately hosted service, invoke Abort without Close/Drop, observe exact process, authenticated/preauth sockets, listener, empty grants and joined owner under one post-return deadline. Inject gateway task join failure with other live/cancelled grants to verify complete drainage. Commits `584b20e`, `aebfa651c6a7865dbba139441113011a8d5fe3eb`. |
| SF-15 — fixed service trait Close ownership | [Retained consumer drainage](../../../examples/kvstore2/src/service.rs#L196-L225), [trait Close](../../../examples/kvstore2/src/service.rs#L372-L388) | `sf15_trait_close_retains_cancelled_consumer_drain` in [read_fencing](../../../examples/kvstore2/tests/read_fencing.rs#L478): actual secondary consumer held at task exit, cancelled Close waiter, retry waits then joins before host shutdown/concrete helper. `trait_close_joins_consumers_and_retains_failure_on_retry` covers diagnostics. Commit `ca0c98ff8e785becf3f1358e5c7053bc3176129b`. |
| SF-17 — bounded current-factory case fixed; broader P7 remains open | [Effective registration settings](../../../kuberic-runtime/src/replicator/mod.rs#L390-L428), [same endpoint in Production](../../../kuberic-runtime/src/replicator/v1/service_adapter/production.rs#L207-L227) | `public_factory_port_zero_effective_settings_are_immutable_and_round_trip` in [classic_v1_production](../../../kuberic-runtime/tests/classic_v1_production.rs#L451): real Default factory/registration/port-zero bind, unchanged effective update succeeds, different/unresolved endpoint rejects, same session/creation/role/address and live listener. Commit `ca0c98ff8e785becf3f1358e5c7053bc3176129b`. |

## Admission and lifecycle boundaries

PostgreSQL samples partition read/write status before its cached lifecycle grant
can bypass checks. Primary application SQL is conservatively admitted only when
both statuses are Granted; a permitted active-secondary read additionally needs
actual replay of the source admission boundary. Physical standby SQL still
rejects writes. A denied previously granted run is hard-stopped, including
preauthentication sockets. Reinstallation of valid lifecycle/configuration and
current partition policy is required to reopen it.

This is periodic owner-driven reconciliation, not a per-statement SQL proxy or
an instantaneous partition-to-HBA transition. Do not treat the partition status
sample itself as proof that existing SQL sockets have already drained. The
regressions observe completed revocation and reject both retained and new access.

The real host retains native classic stable-secondary `NotPrimary` policy.
The isolated secondary readiness test supplies explicit test-only partition
statuses to vary authority independently of replay; it does **not** establish
host-granted secondary reads or add that capability. The primary status-only
case uses actual partition getters and the real host fence. PostgreSQL
legitimately has no StateProvider or StateReplicator.

Orderly drain persists one truthful terminal observation while the original
process generation, synchronous policy and exact sender evidence still exist,
then immediately hard-stops. A stopped former primary exports its certificate,
not raw local flush; a stopped former secondary exports actual receive/replay.
The observation does not certify additional commits racing shutdown, the
shutdown checkpoint, election ranking across arbitrary divergent survivors, or
kernel power loss. Abort does not take a graceful progress snapshot.

Remove retires completed idle ownership, not active voting authority or remote
storage. Cleanup reuses the existing bounded active-sender termination/slot-drop
algorithm and fails explicitly on unreclaimed resources. The dirty obligation
is armed before awaited retirement, so a cancelled caller cannot forget a slot.
Configuration owners include applicable previous/current peers and retained
idle grants. Slot absence proves that retired slot no longer pins WAL; no
production disk-growth bound or general WAL-volume benchmark was measured.

Marked-successor WriteQuorum is rejected rather than approximated by a frozen
WAL boundary. Specific-quorum capability remains false, so capability-false
swaps continue to select All. Ordinary unmarked WriteQuorum and All are supported.
The rejection does not destroy the primary. Native queue arithmetic is not
imposed on PostgreSQL WAL.

Abort synchronously issues cancellation/disarm signals and retains forceful
exact-process cleanup. Gateway listener/connection/task joins are asynchronous;
the ten-second test observation deadline is not a production SLA or a claim that
Abort synchronously joins arbitrary work. Error exits drain the whole cancelled
batch and remaining retained grants rather than bypassing terminal cleanup.

KV trait Close, not only host-exit cleanup, owns consumer cancellation and joins
the self-retained task set. Cancelling a Close future does not detach that set.
Native completion remains distinct from committed KV visibility/client success.
The standalone application-Close test observes its boundary before forceful
fixture teardown; it does not demand native graceful EOF consumption after the
application consumers were already explicitly closed.

Effective settings bind to the actual current factory endpoint at initial
attachment, in both immutable guards. In the pre-P6 **current** factory, original
port-zero input was accepted but the effective address was rejected; this is not
the historical removed-engine trace in which both updates failed. After P6 only
the exact effective address round-trips. There is no original-address alias,
rebind, public settings getter, new defaults, or P7 settings API expansion.

## Native reference versus execution

The read-only native source pin is
`3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d`. Source expectations include
[live partition status](https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/managed/Api/src/System/Fabric/IStatefulServicePartition.cs#L16-L45),
[selected idle session removal](https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Replication/ReplicaManager.cpp#L1783-L1833),
[role-sensitive progress](https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Replication/Replicator.cpp#L296-L352),
and the [marked catch-up predicate](https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Replication/PrimaryReplicator.CatchupAsyncOperation.cpp#L62-L106).
PostgreSQL physical slots/HBA/WAL, idempotent absent-idle Remove, Rust task
joining and the test observation timeout are explicit adaptations, not copied
native algorithms. No native binary/differential execution, native wire
compatibility, RA/FM/placement, deployment-security or production-readiness
qualification follows from these Rust runs.

## Reproduction

Use the repository-pinned Rust/nextest/PostgreSQL tools and existing tiers:

```bash
export TMPDIR="$PWD/target/paw-tmp"
export KUBERIC_POSTGRES_PUBLIC_V1_TMPDIR="$PWD/target/paw-pg"
export CARGO_BUILD_JOBS=2 CARGO_INCREMENTAL=0
export CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_TEST_DEBUG=0
export CARGO_TARGET_DIR="$PWD/target/nextest-ordinary"
mkdir -p "$TMPDIR" "$KUBERIC_POSTGRES_PUBLIC_V1_TMPDIR"
just nextest-postgres-smoke
cargo nextest run -p kvstore2 --all-features --profile ordinary --test read_fencing
cargo nextest run -p kuberic-runtime --all-features --profile ordinary \
  --test classic_v1_production
python3 scripts/validate_nextest_partitions.py
scripts/check_runtime_public_api.sh
```

Executed evidence includes meaningful defect-specific RED/GREEN, routine PG
smoke, affected KV/current factory journeys, protected API/privacy checks and
live exact-once partition validation. Existing incompatible-history, stale
successor, native duplicate, nonblocking Remove and callback-owner controls
remain separate evidence. The full PostgreSQL tier and live Kubernetes tier
were not selected. Consult [Testing](testing.md) for broader qualification;
neither test counts nor a source-token catalog constitutes behavioral closure.
