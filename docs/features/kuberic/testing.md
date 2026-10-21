# Kuberic Test Strategy

Validation is split into exact nextest tiers, workspace/source checks,
provider-backed smoke tests and explicitly owned Kubernetes scenarios.
Commands below are **reproduction entry points**, not claims that every command
was run for the selected-host changes. Read
[current semantics](classic-v1-runtime.md), the
[evidence/disposition ledger](sf-v1-port-ledger.md) and
[source/test provenance](sf-v1-port-provenance.md) for what each evidence class
does and does not establish.

## Recording Evidence

For a qualification claim, record the selected revision, environment, exact
selector/recipe and actual outcome (including skips/failure). Separate:

- inspected source or existing regression body from a newly executed test;
- actual-component, selected public-host and application-file/process evidence;
- source-derived native expectations from native binary/differential execution.

List a proposed filter before running it and reject an empty/misnamed selection.
Use the live inventory rather than fixed test totals. A successful local run
does not erase the [remaining claim-specific gaps](sf-v1-semantic-alignment-plan.md#p2--focused-qualification-not-presumed-defects)
or verify a different mode, fault scope, power-loss boundary or deployment.

## Workspace Checks

```bash
cargo fmt --all -- --check
cargo check --workspace --all-targets --all-features
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --doc --workspace --all-features
scripts/check_runtime_public_api.sh
```

The API guard builds Rustdoc, compares public inventories and compiles external
positive/privacy fixtures ([guard:18–73](../../../scripts/check_runtime_public_api.sh#L18-L73)).
It is not a Markdown link checker. No feature-Markdown site build or repository
Markdown linter/link checker is configured in the inspected baseline; for
docs-only changes inspect relative links/anchors/citation bounds and use
`git diff --check`, without invoking runtime suites merely to edit prose.

## Exact Nextest Inventory

```bash
just nextest-validate
just nextest-list ordinary
just nextest-list source
just nextest-list postgres
just nextest-list kind
```

The [inventory validator](../../../scripts/validate_nextest_partitions.py)
reports live discovery and exact-once tier/partition accounting. Discovery
builds binaries but does not execute PostgreSQL or KinD test bodies; doctests
are additional source-job checks, not archive entries. Repository `just`
recipes may invoke the pinned nextest installer; inspect the
[recipes:16–55](../../../justfile#L16-L55) before using them in a no-install
environment. Do not copy historical discovery/test totals into documentation.

## Ordinary Tier

The ordinary tier is cluster-free but may use real loopback sockets, KV/SQLite
files and owned tasks. Use the repository profile for timeout-sensitive hosted
tests, not an ad-hoc highly parallel `cargo test`.

**Shared-target warning:** `just nextest-test` removes
`target/nextest-ordinary` before running
([recipe:58–69](../../../justfile#L58-L69)). Do not use it with a shared target.
Prefer focused direct selectors or a separately owned target directory:

```bash
cargo nextest run -p kuberic-runtime --all-features --profile ordinary \
  --test classic_v1_production
```

### Classic component evidence

```bash
# All actual private classic owners, not an executable copy of the ledger:
cargo nextest run -p kuberic-runtime --lib --all-features --profile ordinary \
  -E 'test(replicator::v1::)' -j 2

# Example narrower queue selection:
cargo nextest run -p kuberic-runtime --lib --all-features --profile ordinary \
  -E 'test(replicator::v1::operation_queue::tests::)'
```

Current component modules include queue, sender/session/ACK, transport/buffers/
async runtime, primary/secondary, copy/context/BuildIdle, role/recovery,
pressure/ingress and service/host adapters. Select actual module names rather
than historical slice labels. The
[module tree](../../../kuberic-runtime/src/replicator/v1/mod.rs) and
[component provenance](sf-v1-port-provenance.md#component-material-register)
identify owners. A private adapter test does not prove the selected production
host uses every native-shaped policy it models.

### Public factory, host and application evidence

The `classic_v1_production` target includes real factory/host/control/listener,
KV/SQLite-file, external helper-free provider and controller-copy-before-vote
journeys ([tests:1564–2095](../../../kuberic-runtime/tests/classic_v1_production.rs#L1564-L2095)).
Important current source names include:

| Boundary | Existing test / evidence |
|---|---|
| Actual effective endpoint | `public_factory_port_zero_effective_settings_are_immutable_and_round_trip` ([451–502](../../../kuberic-runtime/tests/classic_v1_production.rs#L451-L502)) |
| Independent query outcomes | `native_query_outcome_matrix_survives_rpc_persistence_and_controller_eligibility` and `native_query_not_queried_preserves_copy_detail_and_cannot_elect_or_build` ([991–1168](../../../kuberic-runtime/tests/classic_v1_production.rs#L991-L1168)); observation snapshots are not agent recovery-journal proof |
| SQLite publication | `selected_sqlite_real_wal_isolation_publication_failure_and_restart` ([1731–1801](../../../kuberic-runtime/tests/classic_v1_production.rs#L1731-L1801)) |
| Helper-free provider | `externally_compiled_helper_free_provider_uses_public_factory_host_and_transport` ([1861–1939](../../../kuberic-runtime/tests/classic_v1_production.rs#L1861-L1939)) |
| Native quorum vs service commit | `read_fencing::sf_d02_native_quorum_progress_is_not_a_service_commit_or_caller_cancellation_oracle` ([16–71](../../../kuberic-runtime/tests/support/read_fencing.rs#L16-L71)) |
| Receive vs service ACK | `read_fencing::sf_d02_secondary_receive_progress_is_contiguous_but_does_not_replace_service_ack` ([76–139](../../../kuberic-runtime/tests/support/read_fencing.rs#L76-L139)) |
| Acknowledged prefix | `read_fencing::sf11_acknowledged_prefix_6_through_10_survives_ambiguous_11_without_history_fabrication` ([146–214](../../../kuberic-runtime/tests/support/read_fencing.rs#L146-L214)); not the removed-engine reconciliation fixture |
| KV trait Close | `sf15_trait_close_retains_cancelled_consumer_drain` ([478–532](../../../examples/kvstore2/tests/read_fencing.rs#L478-L532)) |

For example, current exact progress selectors (not the removed durable-scalar
test names) are:

```bash
cargo nextest run -p kuberic-runtime --all-features --profile ordinary \
  --test public_v1_recovery \
  -E 'test(=read_fencing::sf_d02_native_quorum_progress_is_not_a_service_commit_or_caller_cancellation_oracle) or test(=read_fencing::sf_d02_secondary_receive_progress_is_contiguous_but_does_not_replace_service_ack)'
cargo nextest run -p kvstore2 --all-features --profile ordinary \
  --test read_fencing -E 'test(=sf15_trait_close_retains_cancelled_consumer_drain)'
```

Before treating a newly chosen filter as evidence, list it and confirm it
selects the intended current test; discovery alone is not execution. Full
`public_v1_recovery`, `public_v1_session_renewal`, `public_v1_wire` and
`public_v1_conformance` targets cover different lifecycle/journal/wire/structural
boundaries. KSF source/API catalogs are not native behavioral compatibility
certification. Actual wire old-version rejection is not native wire compatibility.

### Selected-host Close and staged access

Targeted runs for the host-alignment revision exercise the real selected
`ReplicaHost` and default public factory. The
[stage witnesses](../../../kuberic-runtime/tests/support/staged_access.rs#L144-L2159)
hold actual executor/callback boundaries and attempt committed service reads
and raw sequence assignment, not only a private selector. They cover joint/
current-only same-primary, missing C/P, split/recipe promotion, current-only/
joint standalone/composed demotion, immediate post-final-wait denial,
cancellation/replay, late reservation/completion and final publication.
They also cover previous-primary-context negatives and same-live-host
repromotion after completed/interrupted demotion, including superseded
promotion/secondary-role/Swap reservations and nonactivating configuration.
Native role/epoch is reconciled only for compatible lineage; fresh role/
configuration/catch-up/activation still controls serving access.
The split-promotion observer-drop witness crosses authenticated control RPC;
other testing-handle entries are selected-host evidence, not all RPC-ingress
qualification.

[Close witnesses](../../../kuberic-runtime/tests/public_v1_recovery.rs#L3114-L3505)
observe child failure, durable diagnostic, outer outcome, Abort and retained
native/provider joins separately. Explicitly seeded recovery records and unit
callback/receipt cuts are not process-crash, power-loss or generic application
task-join proof. Protocol-13 tests cover required availability encoding,
changed-input rejection and deliberate old descriptor-bearing state rejection.
Full-planner unavailable current/capability observations reach actual selected
C/P denial/restoration through public commands. Restored raw admission in that
producer test is not a claim of completed transport/quorum publication.

Inventory the exact filter first, then run it. For checkout-owned temporary
files, set an absolute ignored scratch directory:

```bash
mkdir -p target/host-validation-tmp
export TMPDIR="$PWD/target/host-validation-tmp"
cargo nextest list -p kuberic-runtime -p kuberic-controller --all-features \
  --profile ordinary --ignore-default-filter \
  -E 'test(staged_access::) or test(sf06) or test(availability_) or binary(public_v1_wire) or test(access_approved_assignment_shares_actual_host_revoke_and_final_catchup_cut)'
cargo nextest run -p kuberic-runtime -p kuberic-controller --all-features \
  --profile ordinary --ignore-default-filter -j 1 \
  -E 'test(staged_access::) or test(sf06) or test(availability_) or binary(public_v1_wire) or test(access_approved_assignment_shares_actual_host_revoke_and_final_catchup_cut)'
```

Related visibility/isolation regressions remain `read_fencing::`, the KV
`read_fencing` target and
`selected_sqlite_real_wal_isolation_publication_failure_and_restart`.
Observed bounded Close/fault/renewal wait timeouts during development/review
were followed by passing isolated/serial retries; their root cause is not
established and retries do not erase the reliability limit. No timeout oracle
was weakened. These runs do not close SF-12 controlled rejected-peer ordering,
broad P7, native differential, power-loss or production-readiness qualification.

### In-Process Controller/Application Journeys

```bash
cargo nextest run -p kuberic-controller --all-features --profile ordinary
cargo nextest run -p kuberic-level-tests --all-features --profile ordinary \
  -E 'test(in_process::journeys)'
```

The [in-process harness](../../proposal/in-process-system-test-harness.md)
and [current journey bodies](../../../kuberic-level-tests/src/in_process/journeys.rs)
connect production reconciliation/agent RPCs to real KV services and files;
Kubernetes resource realization and explicit socket cuts are simulated.
They call `KvService` directly, not its HTTP router. They do not replace
Service networking, watch delivery, process pause/resume or owned KinD.

The [downstream guide](in-process-testing.md) documents the optional
`kuberic_controller::test_util` application factory, typed controls and cleanup
contract; the [design](../../proposal/downstream-in-process-testing.md) explains
its ownership boundary. Tests own listeners, tasks and storage and must join
cleanup; a timed-out write remains ambiguous rather than implicit rollback.

## Source-Check Tier

```bash
just nextest-source
```

This checkout/build-tools job runs doctests, Rustdoc/API/privacy/source-policy,
packaged consumer/feature-boundary and registered conformance checks
([recipe:16–24](../../../justfile#L16-L24)).
`nextest-build-only` is a compatibility recipe name, not another execution tier.
Source-sensitive gates need a checkout/build tools even when their binaries
are included in an archive. Their KSF mapping/existence checks are not a
native binary oracle.

## PostgreSQL

Routine separately authorized reproduction:

```bash
just nextest-postgres-smoke
```

The [smoke recipe:90–135](../../../justfile#L90-L135) first asserts the catalog
selection, builds the owned command binary, chooses scratch storage and runs
real PG scenarios serially. Its WSL branch skips for missing pidfd semantics;
a skip is not a pass. Use `just nextest-postgres` only when the full tier is
explicitly authorized, not by default for documentation work.

The actual current catalog is in
[`validate_nextest_partitions.py`](../../../scripts/validate_nextest_partitions.py);
PG implementation/regressions are
[`public_v1.rs`](../../../examples/postgres/src/public_v1.rs),
[`public_v1_tests.rs`](../../../examples/postgres/src/public_v1_tests.rs) and
[`public_v1_p6_tests.rs`](../../../examples/postgres/src/public_v1_p6_tests.rs).
The `_p6` filename identifies existing tests, not today's roadmap cursor.
Evidence distinguishes live-status drain, idle/slot retirement, orderly
terminal progress, marked-successor WriteQuorum rejection, ordinary unmarked
WriteQuorum/All success and retained Abort. See
[current PG semantics](classic-v1-runtime.md#postgresql-custom-path) and
[audit dispositions](sf-v1-port-ledger.md#current-semantic-and-gap-dispositions).
Do not infer native differential, arbitrary survivor election, WAL-growth SLA
or power-loss proof from these process/file observations.

## Archive Reproducibility

```bash
just nextest-archive
just nextest-validate-archive
```

Archive recipes include discoverable all-feature workspace binaries and
repeat exact tier/partition accounting
([recipes:27–40](../../../justfile#L27-L40)).
Archive inclusion does not make checkout-sensitive source tests archive-only;
doctests run separately. Inventory counts are computed, not fixed here.

## Owned KinD Scenarios

Only an explicitly owned disposable cluster may run live scenarios:

```bash
export KIND_CLUSTER_NAME=kuberic-level-dev
mkdir -p target/kuberic-level-dev
export KUBECONFIG="$PWD/target/kuberic-level-dev/kubeconfig"
export KUBE_CONTEXT="kind-${KIND_CLUSTER_NAME}"
# Supply a disposable local token; never reuse or commit production credentials.
export KUBERIC_AGENT_BEARER_TOKEN=local-level-triggered-token

just create-kind-cluster
just level-triggered-images
just level-triggered-install
just level-triggered-kind-test bootstrap replacement failover
just delete-kind-cluster
```

The [ownership recipes:138–210](../../../justfile#L138-L210) require an explicit
receipt and reject default/shared cluster coordinates. Bootstrap/replacement/
failover inspect real deployment resources, authority, data and stale-writer
fencing, with sustained containerd pause/resume in failover; see
[live tests](../../../kuberic-level-tests/tests).
Do not run this tier to validate a Markdown edit or confuse it with native SF
hosted differential, deployment-security certification or production readiness.
