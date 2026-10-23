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

## Engine-neutral hosting

The [bounded hosting map](engine-neutral-hosting.md) distinguishes host
installation, exact remote Build state and optional actual engine observations.
Its real V1/independent PostgreSQL witnesses traverse production controller
dispatch; a manual planner or synthetic companion pass is not that evidence.

On a resource-constrained checkout, use task-owned output rather than deleting
shared build output. Keep the PostgreSQL scratch parent short for Unix sockets:

```bash
owned="$(mktemp -d /tmp/kuberic-hosting-XXXXXXXX)"
export CARGO_TARGET_DIR="$owned/target" TMPDIR="$owned/tmp"
export KUBERIC_POSTGRES_PUBLIC_V1_TMPDIR="$owned/pg"
export CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_TEST_DEBUG=0
export CARGO_BUILD_JOBS=2
mkdir -p "$CARGO_TARGET_DIR" "$TMPDIR" "$KUBERIC_POSTGRES_PUBLIC_V1_TMPDIR"

cargo nextest list -p kuberic-level-tests --lib --all-features --profile ci \
  -E 'test(/^in_process::journeys::actual_v1_/)'
cargo nextest run -j 1 -p kuberic-level-tests --lib --all-features --profile ci \
  -E 'test(/^in_process::journeys::actual_v1_/)'
cargo nextest run -j 2 -p kuberic-level-tests --lib --all-features --profile ci \
  -E 'test(/^in_process::journeys::/)'
just nextest-postgres-smoke
```

The V1 copy witness holds real provider copy while observing engine idle
membership and host InBuild independently, then checks Ready, controller
acceptance/revoke and separate voter admission. Both engines' hosted terminal
witnesses distinguish successful Close, child failure after successful join,
and lost real native joins. PostgreSQL smoke includes actual SQL/replay,
revocation, delayed/pre-effect invalidation and retained lifetime cases.
Direct negative fault injection is setup; production dispatch performs effects.

Common operation/lifetime and controller evidence checks:

```bash
cargo nextest run -j 2 -p kuberic-runtime --lib --all-features --profile ci \
  -E 'test(/^host::operation::tests::/) or test(/^host::public_lifecycle::tests::/) or test(/^replicator::coordinator::tests::/) or test(/^host::hosting::selected_access::access_retry_tests::/)'
cargo nextest run -j 2 -p kuberic-runtime --test classic_v1_production \
  --all-features --profile ci
cargo nextest run -j 2 -p kuberic-controller --all-features --profile ci \
  -E 'test(/^evaluator::public_v1_topology::/) or test(/^cluster_api::tests::renewal/) or binary(=public_v1_evidence) or binary(=public_v1_production) or binary(=public_v1_session_renewal)'
```

Run source/API guards explicitly: nextest filters can exclude their binaries
even when a similarly named selection succeeds. External package proof requires
scratch outside the checkout and is not replaced by a path-dependency check:

```bash
cargo test -p kuberic-runtime --all-features \
  --test public_api_inventory --test lifecycle_capability_boundaries
cargo test -p kuberic-runtime --all-features --test public_api_privacy -- --ignored
scripts/check_runtime_public_api.sh --rustdoc-only
cargo check --locked --offline --no-default-features -p kuberic-runtime --lib
cargo check --locked --offline --no-default-features -p kuberic-controller --bins
cargo test -p kuberic-level-tests --all-features --test source_checks \
  packaged_controller_test_util_boundary -- --ignored
```

These are reproduction commands, not fixed test totals or certification.
Do not substitute the destructive ordinary `just nextest-test` recipe when
shared targets must be preserved. Full PostgreSQL qualification is a separate
explicit decision, not routine smoke.

## Hosting redesign baselines and future acceptance

The [target boundary](engine-neutral-hosting.md#proposed--ordinary-engine-boundary)
and [M1–M3 roadmap](sf-v1-semantic-alignment-plan.md#hosting-redesign-roadmap--not-implemented)
are **NOT IMPLEMENTED**. Commands below select **existing baseline** evidence
at `b37009f8af7a7b2ed91f58f4889ed74e06013861`; they were source-inspected, not
run for this design/docs delivery. A passing current grant/coordinator test
cannot qualify its future replacement. No runtime/PG/build is required for a
Markdown change.

Before a later authorized run, use the identical package/features/profile/
expression with `cargo nextest list` instead of `run`, inspect a nonempty
intended selection, and record revision/argv/environment/outcome and limits.
Discovery can build binaries. Use task-owned output, do not remove shared
targets, and do not run destructive `just nextest-test` or the full PostgreSQL
suite as a docs check.

| Future gate | Existing source-inspected baseline | Missing redesign acceptance (not invented test names) |
|---|---|---|
| M1 host authority/operation retention | [optional diagnostics/remote projection:173–294,394–527](../../../kuberic-runtime/src/host/public_lifecycle_retry_tests.rs#L173-L527); [lost host observer:106–266](../../../kuberic-runtime/src/host/service_cancellation_tests.rs#L106-L266) | Ordinary-only generic bundle without companion; compatible/incompatible action table; scope withdrawal/Abort→Close barrier and missing diagnostics; new production routing |
| M2 engine ingress/lifetime | [carrier/Remove:4407–4611](../../../kuberic-runtime/tests/public_v1_recovery.rs#L4407-L4611); [held provider vs Close/join:351–418](../../../kuberic-runtime/src/replicator/v1/service_adapter/tests/facade.rs#L351-L418) | Engine Active/Retired sessions without grants; delayed first handshake, lost cancel while Primary remains live, scope/effect race, real target quarantine/join and no late successor install |
| M2 public context and controller Build | [workspace provider/contract crate](../../../kuberic-public-v1-tests/Cargo.toml), [actual V1 journeys](../../../kuberic-level-tests/src/in_process/journeys.rs) | Same semantics through new production goal/PrepareIdle/Build route; separate voting/access, no controller-attempt carrier fields |
| M2 real independent PG | [production formation/retained transfer:1257–1570](../../../examples/postgres/src/public_controller_tests.rs#L1257-L1570); [hosted terminal cases:205–378](../../../examples/postgres/src/public_controller_tests.rs#L205-L378) | Ordinary-only factory; native session retirement, lost target cancel/PG helper quarantine, Close-after-Abort settlement, native replay/lineage/SQL fences through new route |
| M3 same-format restart/recovery | [current session/ambiguity reopen:733–835,1476–1586](../../../kuberic-runtime/tests/public_v1_session_renewal.rs#L733-L1586); [hot rollback-journal recovery:1730–1758](../../../kuberic-runtime/tests/public_v1_session_renewal.rs#L1730-L1758) | Two-ended Ready revalidation after target reopen/lost reply; source/target/controller selection/effect/receipt crashes, acknowledged-prefix/PC/CC/access proof; recover matching-format state without resetting it |
| M3 clean deployment and removal | Existing [source/API/package commands](#engine-neutral-hosting), schema-rejection selection below | Replacement property coverage before each API deletion; no superseded production path remains; incompatible stores/commands/CRD intents reject before mutation; no old-state import, upgrade/downgrade or automatic data/PGDATA reset |

Focused current selectors:

```bash
# Host facts and response-independent execution (M1 baseline):
cargo nextest run -p kuberic-runtime --all-features --profile ordinary \
  -E 'test(/host_install_survives_unavailable_and_invalid_optional_idle_projection/) or test(/remote_build_projection_separates_admission_running_success_and_retirement/) or test(/cancelled_configuration_request_does_not_strand_durable_completion_behind_access_fence/)'

# Built-in carrier/Remove and held provider lifetime (M2 baseline):
cargo nextest run -p kuberic-runtime --all-features --profile ordinary \
  -E 'test(=production_carrier_rejects_crossed_process_storage_history_epoch_and_endpoint) or test(=native_remove_cancels_session_without_late_install_and_close_drains_retained_provider) or test(/close_finishes_before_held_build_provider_but_join_and_cancelled_build_stay_rooted/)'

# Current actual built-in controller path, not a native SF executable:
cargo nextest run -p kuberic-level-tests --lib --all-features --profile ordinary \
  -E 'test(/^in_process::journeys::actual_v1_/)'
```

The [public target-origin context section](#public-target-origin-copy-context)
owns package-only versus workspace feature proof and current wrapper locations.
Do not use a removed `scripts/fixtures/classic-v1-provider` path as a current
command; pinned historical links keep original provenance.

```bash
# Actual independent PG controller formation/invalidation (M2 baseline):
cargo nextest run -p postgres-replicated --all-features --profile postgres \
  -E 'test(=adapter::public_v1::tests::controller::public_v1_postgres_controller_formation) or test(=adapter::public_v1::tests::controller::public_v1_postgres_controller_delayed_build_invalidation) or test(=adapter::public_v1::tests::controller::public_v1_postgres_controller_pre_effect_invalidation)'

# Actual PG terminal outcome versus real native join (M2 baseline):
cargo nextest run -p postgres-replicated --all-features --profile postgres \
  -E 'test(=adapter::public_v1::tests::controller::public_v1_postgres_controller_terminal_success) or test(=adapter::public_v1::tests::controller::public_v1_postgres_controller_terminal_child_close_failure) or test(=adapter::public_v1::tests::controller::public_v1_postgres_controller_terminal_lost_native_join)'

# Current same-format recovery, restart ambiguity and prior-schema rejection (M3 baseline):
cargo nextest run -p kuberic-runtime --all-features --profile ordinary \
  -E 'binary(=public_v1_session_renewal) and (test(=public_v1_replacement_schema_build_ambiguity_requires_retirement) or test(=public_v1_replacement_schema_failed_build_containment_survives_reopen) or test(=public_v1_replacement_schema_recovers_hot_rollback_journal) or test(=public_v1_replacement_schema_rejects_prior_preview_stores))'
```

Current common-coordinator negatives remain useful property inventories:
`test(/replicator::coordinator::tests::/)` with runtime all-features/ordinary.
They use controlled effects, not actual PG/kernel cleanup. Remote-state tests
seed records; facade tests use Rust provider fixtures; PG controller cases use
real PG plus the common harness, **not Kubernetes**. Direct PG future-drop tests
are not the hosted response-observer contract. Current failed-Build containment
does not prove automatic arbitrary reformation.

For docs-only validation use `git diff --check`, changed Markdown relative-path/
heading checks and source-anchor bounds against exact native/current/pinned
historical blobs. There is no configured Markdown site build/link checker;
do not install a framework or invoke Rustdoc guards solely to validate prose.
Native source inspection is not native executable evidence. MySQL, production
security, power-loss and exhaustive schedule/mode qualification remain separate.

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

### Public target-origin copy context

The [shared external-provider scenario][public-context] at
`45749d3744b43095be9bc9ec3fb4abf20633a537` actually passed locally both as an
independent fixture test and through `classic_v1_production`. It consumes
`[b"target/context", empty, b"target/trailer"]` through EOF at the real nonzero
pre-Build committed boundary; the source advertises different bytes and never
acquires its own context. A fresh target installs checked application values
distinct from the context. Exact successful Build completion, remote Ready,
public Close, actual host waits and provider write/stream drainage are checked.
The target remains idle outside voters with serving reads denied; file
inspection is not admission or a serving read.

The provider and scenario now live in the root-workspace member
[`kuberic-public-v1-tests`](../../../kuberic-public-v1-tests/Cargo.toml).
Its integration test is discovered directly, without the former path-imported
runtime wrapper. Package-only selection enables runtime `host` without
`testing`; workspace-wide dev dependencies unify `testing`, so a workspace
run alone is not normal-feature proof. Checkout `kuberic_workspace_tests` cfg
is a different boundary again. The pinned results above describe the original
fixture location, not the current crate layout.

Reproduction commands below are not additional execution claims. Reserve a
previously absent, task-owned ignored output directory with sufficient disk
capacity for separate package/workspace builds. Stop if reservation fails;
do not delete or reuse another task's output. Run hosted selections serially,
retain exact argv/status/raw output and source/manifest/lock hashes, and do not
use `just nextest-test` or remove shared `target`:

```bash
set -euo pipefail
owned="$PWD/target/public-copy-context-local"
test ! -e "$owned"
mkdir -p "$owned/package" "$owned/workspace" "$owned/scratch"
export TMPDIR="$owned/scratch" CARGO_TARGET_DIR="$owned/package"
export CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=2
export CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_TEST_DEBUG=0
unset NEXTEST_ARCHIVE
package=kuberic-public-v1-tests

cargo tree --offline --locked -p "$package" -e features -i kuberic-runtime
cargo nextest run --offline --locked -p "$package" --all-targets --no-tests fail
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo fmt --all -- --check
```

The [CI step][public-context-ci] runs the whole public-test crate suite without
test-name filters; nextest fails if no tests run. `cargo tree` logs the resolved
feature graph for inspection, rather than enforcing a custom metadata policy.
The suite retains the public Build and provider controls for wrong, missing,
reordered or surplus context, stream errors, empty-item/EOF distinctions and
the default empty-context policy. The new member is covered by the existing
workspace Clippy/format jobs; the package-only step need not duplicate them.
Existing source/API/package guards remain separate and unchanged.

For workspace regression coverage:

```bash
export CARGO_TARGET_DIR="$owned/workspace"
filter='test(=externally_compiled_helper_free_provider_uses_public_factory_host_and_transport) or test(=replicator::v1::copy_establishment::tests::persisted_target_context_flows_through_actual_receivers_and_senders) or test(=replicator::v1::copy_establishment::tests::nonpersisted_target_never_calls_get_copy_context) or test(=replicator::v1::copy_async_operation::tests::empty_data_is_an_item_and_provider_is_never_polled_after_eof)'
cargo nextest list --offline --locked -p kuberic-runtime --all-features --lib \
  --test classic_v1_production --profile ordinary --message-format json -E "$filter"
cargo nextest run --offline --locked -j 1 -p kuberic-runtime --all-features --lib \
  --test classic_v1_production --profile ordinary --success-output immediate -E "$filter"
```

Verify exact nonignored ordinary selection before running. The workspace
selection also passed the unchanged default empty-context public journey and
persisted/volatile/empty-item component regressions; these remain separate
claims. [KSF-05 registration][public-context-ksf] is additive. Nonignored KSF
catalog/mapping/partition/mutation guards passed, not the ignored broad nested
scenario executors or a locally rebuilt whole-workspace archive.
The package-only CI step preserves normal-feature execution separately from
workspace/archive runs, which now discover the public-test crate directly.
Local execution is not a remote CI result.

Actual separate source/API/privacy/Rustdoc, production runtime/controller
feature-off and vendored packaged-consumer gates also passed, along with
targeted Clippy/formatting. The initial standalone Clippy failed on a
preexisting fixture `collapsible_if`; the equivalent condition was repaired and
the same gate passed, without runtime production/interface changes.
Host-only runtime dependency warnings remain distinct from fixture lint
success. Reproduce the nonignored conformance/source selection explicitly:

```bash
env -u UPDATE_RUNTIME_SOURCE_API -u UPDATE_RUNTIME_CONTROL_SOURCE_API \
  cargo test --offline --locked -p kuberic-runtime --all-features \
  --test public_api_inventory --test lifecycle_capability_boundaries --test public_v1_conformance
cargo test --offline --locked -p kuberic-runtime --all-features \
  --test public_v1_evidence public_v1_source_uses_no_native_evidence -- --exact
cargo test --offline --locked -p kuberic-runtime --all-features \
  --test public_v1_recovery production_config_has_no_protocol_selector_or_application_commit_oracle \
  -- --exact
```

Separate privacy/Rustdoc/feature-off/package reproduction uses the
[commands above](#engine-neutral-hosting). The package gate requires
separately authorized, task-owned **outside-checkout** scratch supplied via
`TMPDIR` for that command; the checkout-owned scratch above cannot satisfy its
isolation assertion. Its packaged test closure is compiled, not an execution
of this new scenario from a published package. Verify fixed compiler-fixture
path ownership/concurrency before source guards, and clean only proven owned
paths after their processes finish.

Provider negative controls are not public network-fault/failed-Build recovery
tests. No PostgreSQL, broad suite, cluster, native executable/differential,
power-loss or production-certification result is implied. See
[current semantics](classic-v1-runtime.md#copy-and-build) and
[source ancestry](sf-v1-port-provenance.md#public-target-origin-copy-context-qualification).

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
was weakened. Those Close/access runs do not establish rejected-peer ordering. The separate
scoped rejection evidence below does not explain the earlier timeouts or close
broad P7, native differential, power-loss or production-readiness qualification.

### Scoped rejection-order and local-fault evidence

The current focused actual-owner and public-host vectors passed under the ordinary profile.
Use the live names, not a fixed total:

```bash
cargo nextest list --offline -p kuberic-runtime --all-features \
  --profile ordinary --lib --test public_v1_recovery -E 'test(rejection_order::)'
cargo nextest run --offline -p kuberic-runtime --all-features \
  --profile ordinary -j 1 --lib --test public_v1_recovery -E 'test(rejection_order::)'
```

The [owner vectors](../../../kuberic-runtime/src/replicator/v1/primary/tests/rejection_order.rs#L18-L230)
observe failed idle C Build and genuine B progress in both orders, preserve
current/previous/marked/All requirements, and reject new assignment/cached-ready
catch-up after GetCopyState/EndGetNext faults. Active copy error is a separate
replication-progress case, not a generic NACK.
The [public vectors](../../../kuberic-runtime/tests/public_v1_recovery/rejection_order.rs#L57-L392)
use actual C-listener configuration denial, exact metadata/status forwarding
and source retry entry as a consumption barrier. B's service-owned persistence/
ACK is withheld across real issued work and captured-boundary marked catch-up;
healthy-first completes native/application work before C is released.
A [same-native-wait handshake](../../../kuberic-runtime/tests/public_v1_recovery.rs#L590-L625)
polls the real wait, crosses an ordered same-facade query and checks that
future Pending before healthy release; rejection-first probes it again after
source consumption. Wrapper entry or unfinished outer journal work is not
the native-pending assertion.
A further real write demonstrates the source is not poisoned after rejection.
The local-source Build-fault vector exercises both acquisition/enumeration
origins through the selected host/public native interfaces.

Representative existing controls also passed in the focused combined run:
PC/CC and marked/live-Q manager predicates; required service ACK and primary
commit failure; healthy quorum/hung minority; host identity rejection; malformed
live ACK, retired ACK and unexpected worker fault distinctions. These controls
are not substitutes for the new active-rejection ordering vectors.
Scoped runtime clippy, formatting and the protected source/API guard passed.
No PostgreSQL, whole-workspace, native executable or cluster qualification is
implied. See [fault scope](classic-v1-runtime.md#rejection-and-fault-scope),
the [ledger](sf-v1-port-ledger.md) and
[source ancestry](sf-v1-port-provenance.md#scoped-rejection-and-primary-local-fault-entry--f02f07f08f11).

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
The independent
[`public_controller_tests.rs`](../../../examples/postgres/src/public_controller_tests.rs)
uses production PgService, native PostgreSQL processes/RPCs and real controller
dispatch. It is not a raw built-in bundle wrapper or a direct native-only Build
fixture. Its selected names cover formation, delayed source/target invalidation
and cleanup failure; the smoke catalog includes them.
The `_p6` filename identifies existing tests, not today's roadmap cursor.
Evidence distinguishes live-status drain, idle/slot retirement, orderly
terminal progress, marked-successor WriteQuorum rejection, ordinary unmarked
WriteQuorum/All success and retained Abort. See
[current PG semantics](classic-v1-runtime.md#postgresql-custom-path) and
[audit dispositions](sf-v1-port-ledger.md#current-semantic-and-gap-dispositions).
Do not infer native differential, arbitrary survivor election, WAL-growth SLA
or power-loss proof from these process/file observations.

### Custom-native controller reproduction and limits

Keep both scratch roots owned by the checkout, inventory the live selector,
then run the authorized smoke entry point:

```bash
mkdir -p target/paw-tmp target/postgres-public-v1
export TMPDIR="$PWD/target/paw-tmp"
export KUBERIC_POSTGRES_PUBLIC_V1_TMPDIR="$PWD/target/postgres-public-v1"
just nextest-postgres-smoke-inventory
filter="$(python3 scripts/validate_nextest_partitions.py --postgres-smoke-filter)"
cargo nextest list -p postgres-replicated --all-features --profile postgres -E "$filter"
just nextest-postgres-smoke
```

The actual controller witness observes successful fresh Open and explicit
unknown-history sentinels before first Role; nonempty SQL before scaling;
real source/target backup, lineage, receive/replay and built-idle state; separate
role/epoch, exact joint catch-up and current-only admission; then synchronous
SQL/replay and retained-socket rejection after completed native fencing.
Live native retry and independently mutated authority checks cannot repeat
copy mutation. Supported source/target fault reporting at held native cuts
rejects delayed completion. Cleanup errors remain errors while actual owners
are joined. Native companion stale-revoke calls are explicitly component
negatives, not substituted controller positives.

Overlapping native RPCs attach before mutation ownership; dropping a queued
observer cannot cancel the transfer or clean up a later admitted peer. Separate
source/target pre-effect cuts reject before slot/database preparation. The
direct protected-Close preservation case checks acknowledged WAL beyond the
prior durable certificate without an intervening explicit snapshot; it is
not labeled controller dispatch. Actual-host runtime cuts separately suspend
authority reads/commit, then fence, cancel or revoke before release.
Another direct protected-Close case holds genuine source Inspect responses
during target preflight and cached replay, requiring bounded interruption
without releasing the response or invoking Abort first. A scoped slot-retry
helper negative injects a retryable first response, invalidates a genuinely
hosted source and rejects the next mutation round; that injected helper reply
is not database transfer/SQL success evidence. Runtime unit coverage separately
holds compensation while shutdown observers overlap/disappear and checks
shared sticky failure plus once-only successful application drainage.
Native ownership negatives additionally cancel the genuinely accepted source
Build after gateway spawn, hold actual gateway exit through terminal finish,
and check exact listener/accepted-socket drainage. A lost native Close observer
cannot erase the retained provisional gateway's failed join. Another case
loses the exact hosted inbound task and repeats protected/companion Close:
the same failed task join remains an explicit error, all other owners join,
and consuming finish stays unsuccessful instead of panicking or fabricating
containment. These fault injections are not substitutes for the positive
controller data-transfer/admission journey.
The source cut includes an existing admitted native peer plus the provisional
new gateway. Losing Close while one exit is held cannot detach the other.
The failure variant consumes a failed first join, loses that next Close
observer while the second join is still held, then requires the first error
to survive actual completion of the second join and consuming finish.

The separate
[external runtime contract fixture](../../../kuberic-runtime/tests/support/custom_control.rs)
uses an independently attached engine, real host journals/control dispatch and
owned child tasks, but deliberately has no native database/quorum implementation.
Its truthful progress-unavailable and unsupported wait results must not be
called a native positive witness:

```bash
cargo test -p kuberic-runtime --features testing --test classic_v1_production custom_host_
cargo test -p kuberic-runtime --features testing --lib replicator::coordinator::tests
cargo test -p kuberic-runtime --test public_api_privacy -- --ignored
```

The ignored privacy test must actually be selected. Neither these contract
fixtures nor direct PG tests alone prove custom-controller compatibility.
This bounded upstream PG evidence is not unpublished downstream MySQL
requalification, the reported seven-completion trace, arbitrary failed-Build
reformation, native executable differential or Kubernetes deployment evidence.

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

[public-context]: https://github.com/youyuanwu/kuberic/blob/45749d3744b43095be9bc9ec3fb4abf20633a537/scripts/fixtures/classic-v1-provider/tests/public_copy_context.rs#L377-L647
[public-context-ksf]: https://github.com/youyuanwu/kuberic/blob/45749d3744b43095be9bc9ec3fb4abf20633a537/kuberic-runtime/src/testing/public_v1_conformance.rs#L905-L918
[public-context-ci]: ../../../.github/workflows/CI.yml
