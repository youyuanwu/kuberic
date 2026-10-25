# Kuberic Test Strategy

Validation is split into exact nextest tiers, workspace/source checks,
provider-backed smoke tests and explicitly owned Kubernetes scenarios.
Commands below are **reproduction entry points**, not claims that every command
was run for the selected-host changes. Read
[current semantics](classic-v1-runtime.md), the
[evidence/disposition ledger](sf-v1-port-ledger.md) and
[source/test provenance](sf-v1-port-provenance.md) for what each evidence class
does and does not establish.

**Current checkpoint:** [ordinary hosting M1](ordinary-hosting-m1.md) is
implemented. Bounded M2 implementation acceptance passed for built-in V1,
KV, SQLite, the standalone external provider and independent PG native sessions,
effects, admission and actual joins through production ordinary goals.
Documentation implementation review and actual independent final Sol/Astra
gates have not run; the complete M2 workflow and exhaustive M3 remain separate.
Built-in historical bodies are enabled rather than returning an unavailable
success. Their broader recovery/handoff results are not bounded M2 acceptance.
Actual engine/component negatives, application witnesses and production
controller journeys remain distinct selections.

## Built-in M2 engine slice

`classic_v1_production`'s `m2_builtin_` cases use authenticated semantic goals,
real default-factory source/target engines and native carrier/provider/ACK owners.
They cover target context and empty items versus EOF, copy and voting-replication
ACKs, bounded burst confirmation and synchronous duplicate rejection, retired
first establishment, lost cancellation/replies, effect/await/delivery/credit
fences, retained quarantine, identity-qualified stale ACK rejection, bounded
creation-lifetime tombstones and Abort → actual Close joins. Multi-secondary
cases exercise fresh and subsequent joint/current admission, actual voting
ACKs and all-member catch-up with unchanged exact sessions. Target Retire
holds session-qualified queued/in-flight ACK joins across lost RPC observers;
clean cancellation grants no native credit and actual ACK-owner failure stays
sticky. Physical dial and
retained-task cuts are testing-only; diagnostics never authorize work.

The bound body-epoch witness keeps authenticated carrier input live and
correlates each injected frame with the actual retained ingress task's rejection.
Before teardown it polls the real copy/replication streams and compares native
receive/apply queues, application bytes, provider acquisitions and ACK credit.
StartCopy is tested before initialization, copy after legitimate StartCopy, and
replication after actual copy, joint/current admission and an acknowledged
replication prefix. Read-only, bounded ingress/receiver diagnostics are private
to the concrete testing owner; neither RPC EOF nor unchanged ACK credit alone
establishes no delivery.

```sh
cargo nextest list -p kuberic-runtime --all-features --profile ordinary \
  --test classic_v1_production -E 'test(/^m2_builtin_/)'
cargo nextest run -p kuberic-runtime --all-features --profile ordinary \
  --test classic_v1_production -E 'test(/^m2_builtin_/)' --no-tests fail
```

Private `service_adapter` repair fixtures remain component evidence, not ordinary
host acceptance. These bounded in-process engine tests alone do not qualify
application services, PostgreSQL, live clusters or M3 recovery.

## Built-in M2 application slice

`classic_v1_production`'s `m2_application_` cases execute real KV, SQLite and
external-provider Open/copy/access and acknowledged-data reopen. They also hold
actual application consumers and a retained SQLite blocking SQL worker through
ordinary Abort/Close, retain a failed consumer join, and reject incompatible
goals before application or authoritative-command mutation. The separate role
receipt witness checks actual persisted application callback bytes on reopen.

The standalone `kuberic-public-v1-tests` package must also run without workspace
feature unification. Its ordinary copy-context body checks target-origin
nonempty/empty/nonempty items, EOF and a meaningful durable copied snapshot,
not context bytes mistaken for application data.

Production-controller witnesses include
`production_ordinary_builtin_copies_actual_application_bytes_before_admission`
and the level suite's `in_process::journeys::actual_v1_` selection. They use
canonical ordinary snapshots and real copied bytes, not required idle diagnostics,
installed grants or callback-program dispatch.

```sh
cargo nextest list -p kuberic-runtime --all-features --profile ordinary \
  --test classic_v1_production -E 'test(/^m2_builtin_/) or test(/^m2_application_/)'
cargo nextest run -p kuberic-runtime --all-features --profile ordinary \
  --test classic_v1_production -E 'test(/^m2_builtin_/) or test(/^m2_application_/)' --no-tests fail
cargo nextest list -p kuberic-public-v1-tests --all-targets --profile ordinary
cargo nextest run -p kuberic-public-v1-tests --all-targets --profile ordinary --no-tests fail
```

Full historical replacement, promotion, data-loss and renewal bodies remain
enabled diagnostic journeys. Record their actual failing clause and unexecuted
remainder separately; neither a red aggregate nor an unsupported old fixture is
a passing M2 witness. Required M2 assertions pass separately, as recorded below;
final workflow reviews remain required.

## M2 accepted evidence and remaining boundaries

This is the bounded implementation evidence accepted by independent Phase 6
review, not a new execution by this documentation update or a final M2 review.
Use the live inventory, never a fixed total. Historical source pins and failed
attempts retain their original outcomes.

| Source binding | Actual accepted execution / outcome |
|---|---|
| Phase 6 code `84ff69cf5790f6b41e82c352fe03c39a40fc7d7a`, tree `85a6f280d0fc64a3b691090ad248834fbbb75d78` | Native V01 `75139c85-aeb8-44f4-a29e-156e75f9de94`, ordinary V02 `3806f222-a8aa-4864-bf8b-784edaf97304`, actual application V03 `dc3b866b-08d8-4260-9de6-1a00ff8cf41a`, required source/API/privacy/package V11A `a6a495f8-3fe2-4b95-8a9a-9b063079e0b7`: PASS. Format, scoped runtime/controller/KV all-target/all-feature check and strict scoped clippy, protected whole files and exact partition validation: exit 0. This is not whole-workspace strict-lint or warning-free normal-feature compilation. |
| Same final code, independent reviewer | Complete bounded controller `1ade72c6-21d1-41e0-b80e-5895886536c5` and real KV growth `2daef79b-0c97-4c4f-8d83-b7bc4152a08e`: PASS. Achieved predecessor Close no longer vetoes the fresh current owner; actual current Abort/Close still vetoes Build regardless of reported status. Real `retained`/`later`/`after-growth` KV data, actual native copy, exact acceptance and separate admission execute. |
| Recovery code `af06768a4e5dbc8ef2a57935e0b12342a2c540eb`, tree `4b28368fe09373b7eb43a41fef5e8de8186161d5` | Standalone normal-feature provider `a33702d5-224d-4df6-b1f9-9f95d08da25a`, actual level V06 `c7e25722-5318-4ca7-8583-9e7b1939da1d`, oracle `ceef1f3d-8eb1-4a6a-8451-f24ee2ca9da9` and authorized routine PG smoke `7f8131ed-acb2-4e32-b6d5-a842c45b5765`: PASS. Their runtime/PG/host owners are unchanged by the later controller repair; they were not rerun there. No full PG run. |
| Same recovery code, owned matching runtime-only images | Full bootstrap `d96c32a3-61e0-4d56-aa10-8e19b175254d`: PASS, exit 0, including actual secondary reconstruction, renewed exact authority and final write/read. Source/binary/native-library/OCI/pod/owned-context custody was independently verified. **The image predates the final controller repair; no live pass or image equivalence for `84ff69cf…` is claimed.** |

These historical runs also predate the final-review built-in engine fixes.
They are not live-image or current-binary evidence for recovery reservation,
exact predecessor compare-and-take, or serialized idle Remove. The new
`final_remediation::m2_recover_` native regressions use actual authenticated
Recover RPCs, copy, carrier/ACK ownership, quorum writes and joins. The ordinary
`m2_builtin_remove_serializes_with_native_configuration_adoption` witness
executes both adoption/Remove orderings; the multi-secondary witness rejects
voter Remove before its real quorum and All catch-up assertions.
Final independent review remains a separate gate.

The retained-member recovery proof requires an unchanged available Primary and
exact storage/history/epoch/configuration. It executes fresh authenticated
session establishment, old carrier/effect/ACK joins, full retained-prefix
context and nonregressing actual provider transfer before exact source
configuration/catch-up and target callback readmission. Transfer is nonvoting
before admission. The singleton and subsequent KV growth proof is separate
from general election/reformation. The duplicate-Discover, incomplete-copy and
stale-terminal veto defects were M2 bugs, now fixed—not remaining M3 waivers.

Red gates remain enabled and their reached/unexecuted clauses remain explicit:

| Gate / actual outcome | Reached boundary and limit |
|---|---|
| V11B unchanged `just nextest-source`, `5dc4b63d-9a5b-4033-83b4-38c56a6ea2ac`, exit **100** | Doctest/Rustdoc stages exit 0. Native child exhausted shared work budget: SIGTERM, child reaped, pipes drained, **no underlying Cargo exit recorded**. This is infrastructure termination, not an M3 test pass. The complete identical-selection native complement supplies missing local-commit/acknowledged-prefix outcomes on its own source; it does not rewrite the aggregate. |
| V14 final native complement `396d6f7d-c87d-4b8c-a260-b75b43cef69e`, exit **100** | Required native M2 bodies pass. Singleton restart now passes actual fresh growth, then secondary deletion/replacement hits a shared deadline with `observed Kubernetes object changed; a fresh observation is required`. Survivor healing does not settle. Retained-secondary promotion and swap reach explicit planned-primary-replacement rejection. These are distinct diagnostics, not successful replacement/election or blanket “M3” by test name. |
| Full V05 final application selection `66c95c8d-1428-4cd8-98fd-bd5ebdd0393b`, exit **100** | SQLite changed-primary same-live-creation handoff rejects `incompatible scope requires whole-creation withdrawal`, after actual copy/data/reopen. Its suffix is unexecuted. Required M2 application/recovery cases pass separately. |
| V11B service ambiguous-commit/abort `72068e1a-eb82-4a38-996e-2cd6000359a2`, exit **100** | Acknowledged prefix, commit isolation and owner stop execute; subsequent heal/recovery fails to settle with observation/CAS diagnostics. No later successful failover credit. |
| V11B failover-after/at durable promotion `d87d818e-4855-4fd0-9c81-f906dea62809` / `37bb8908-64c8-4044-9227-4d18158ab20b`, exits **100** | Baseline/predecessor isolation and deadline/observation-CAS diagnostics; promotion/completion-once/recreation suffixes receive no credit. |
| V11B switchover-after-catch-up/durable-handoff `87d19a18-928a-432f-8bfe-1139ef4d7e8b` / `9070e668-bf09-4d0f-914f-7411081a7604`, exits **100** | Explicit planned-primary-replacement rejection; accepted handoff/recreation/promotion suffixes receive no credit. |

No all-source, historical-journey, CI-green, native executable equivalence,
security/orphan or production-readiness claim follows. Required source/API/
package assertions are V11A; V11B still executes real nested ordinary/native
journeys and propagates nonzero results. No positive profile, mapping or CI
gate was disabled.

### Reproducing the bounded selections

Use repository-pinned Rust/nextest and `protoc`; PG additionally requires
unprivileged Linux, compatible local binaries, pidfd/subreaper facilities and
short owned socket paths. Reserve an unused checkout-owned ignored root; never
delete shared build outputs. The accepted runs used executable task-owned
`/dev/shm` build/scratch roots with `CARGO_INCREMENTAL=0`,
`CARGO_PROFILE_DEV_DEBUG=0`, `CARGO_PROFILE_TEST_DEBUG=0`,
`CARGO_NET_OFFLINE=true`, `RUST_BACKTRACE=0`; PG data used `.paw/m2pg`.
Record literal paths, versions, source maps, selection, raw streams and exit.
These commands are reproduction guidance, not executions by this phase:

```bash
owned="$PWD/target/m2-validation-owned"
test ! -e "$owned"
mkdir -p "$owned/build" "$owned/scratch" "$owned/pg"
export CARGO_TARGET_DIR="$owned/build" TMPDIR="$owned/scratch"
export KUBERIC_POSTGRES_PUBLIC_V1_TMPDIR="$owned/pg"
export CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_TEST_DEBUG=0
export CARGO_NET_OFFLINE=true RUST_BACKTRACE=0
filter='test(/ordinary_goal/) or test(/production_ordinary_/) or test(/m2_ordinary_/)'
cargo nextest list --offline --locked -p kuberic-runtime -p kuberic-controller \
  --lib --all-features --profile ordinary -E "$filter"
cargo nextest run --offline --locked -p kuberic-runtime -p kuberic-controller \
  --lib --all-features --profile ordinary -E "$filter" --no-tests fail
source_filter='not (binary_id(=kuberic-runtime::public_v1_conformance) and test(/^public_v1_conformance_executes_/))'
cargo nextest list --offline --locked --workspace --all-features \
  --profile source --run-ignored all -E "$source_filter"
cargo nextest run --offline --locked --workspace --all-features \
  --profile source --run-ignored all -E "$source_filter" --no-tests fail
```

Run the [actual engine](#built-in-m2-engine-slice), [application/provider](#built-in-m2-application-slice)
and [routine PG smoke](#custom-native-controller-reproduction-and-limits)
selections independently. Inspect each identical live list before execution.
The V11A subset above is not a replacement for `just nextest-source`, doctests,
Rustdoc or the exact source/API/privacy binaries. Full PG needs explicit
authorization. Do not rerun runtime/native/lint suites solely for prose.

### Ordinary downstream and live boundaries

The bootstrap conflict witness holds an actual ordinary-goal outcome status
write, injects a metadata-only resource-version conflict and checks the exact
retained goal/digest after retry. Fresh-build transport isolation and teardown
use actual membership growth and engine dials, not legacy callback programs or
an unsupported planned handoff. Reference validation reads the local process's
exact storage/session/creation identity; it does not mistake an isolated RPC
endpoint for an obsolete reference. Readiness still requires real RPC evidence.

The downstream counter's bounded secondary-gap witness checks unchanged ordinary
membership, an unavailable secondary observation and an actual healthy-quorum
write/read. Receiver validation uses authenticated confirmation of the source's
actual private peer bindings, not reachability of every unrelated receiver.
The full automatic-restart gap journey remains separately enabled.

Ordinary reconfiguration omits an absent `catchUp` selection rather than
serializing an enum-valued null rejected by the Kubernetes API server. This
does not alter selected catch-up modes or admission policy.

The owned KinD bootstrap body checks exact three-member sessions, actual
secondary reconstruction and final quorum write/read; its full accepted pass
is bound above to the recovery image source. Separate replacement and failover
bodies remain enabled and unqualified. A formation prefix alone is never a
passing full live journey. Runtime-only local images
must contain hash-verified binaries built from the recorded source, with no
unconfigured nested Rust build. Never appropriate a shared cluster or reset
retained application data to make an incompatible deployment pass.

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

## Ordinary hosting M1

Actual M1 production and host selections were executed with an independent
ordinary factory, real listeners, authenticated agent transport and locally
retained callbacks. Production uses an in-process Kubernetes storage model with
generated structural-schema status pruning, not a live API server.

```sh
cargo nextest list -p kuberic-controller --all-features --profile ordinary \
  -E 'test(/production_ordinary_/)'
cargo nextest run -p kuberic-controller --all-features --profile ordinary \
  -E 'test(/production_ordinary_/)'
cargo nextest list -p kuberic-runtime --lib --all-features --profile ordinary \
  -E 'test(/ordinary_goal/)'
cargo nextest run -p kuberic-runtime --lib --all-features --profile ordinary \
  -E 'test(/ordinary_goal/)'
```

They cover held multi-replica recovery/progress and mutual discovery under closed
copy/voting/serving, Open/receipt gaps, retained/lost responses, nonempty fixture
copy and exact Ready before separate admission, stale/two-ended publication,
failed preparation, actual engine/service/host joins and provider-writer fencing,
negative generations/bounds and same-format interrupted-call uncertainty.
The crash-image fixture does not claim process-kill or power-loss proof.

Validation also executed workspace format/check/strict clippy/build/doctests,
controller/native-component/host regressions, generated CRD, protected method/
source/API/privacy/package gates, normal-feature external package and authorized
PostgreSQL smoke. Each nextest selection was live-listed first. No full PG,
destructive `just nextest-test`, shared-target deletion, old-state import/data
reset, native executable or KinD run was used.

Real native/component algorithms remain separately exercised with
`-p kuberic-runtime --lib --all-features --profile ordinary -E 'test(/^replicator::/)'`;
list before run. Routine PG smoke includes actual ordinary native/controller
witnesses, not unavailable-success wrappers. Historical direct/native recovery
diagnostics remain selected separately from replacement-session acceptance.
Remaining workflow gates and M3 work are in the
[roadmap](sf-v1-semantic-alignment-plan.md#hosting-redesign-roadmap).

Task-owned executable build/scratch storage can be selected with
`CARGO_TARGET_DIR` and `TMPDIR`; do not delete shared outputs to make room.
The M1 execution used owned `/dev/shm` directories on a Linux host where that
mount was executable, plus disabled incremental/debug output. This is resource
placement, not permission/trust bypass. Use short owned PG scratch paths and
retain exact command/environment/revision/outcome evidence.

## Engine-neutral hosting

**Pinned pre-M1 evidence and reproduction context**, through
`ce9df4ad85603b935752ed7184d9314a778b6eb5`. The native/component properties
below remain relevant, but their old production grant/controller witnesses are
not current M2 positives. Legacy-owner commands in this section apply only at
that historical checkout, not today's removed coordinator/API. Use the
[current M2 selections](#m2-accepted-evidence-and-remaining-boundaries) instead.

The [bounded hosting map](engine-neutral-hosting.md) distinguishes host
installation, exact remote Build state and optional actual engine observations.
Its real V1/independent PostgreSQL witnesses traverse production controller
dispatch; a manual planner or synthetic companion pass is not that evidence.

On a resource-constrained checkout, use task-owned output rather than deleting
shared build output. Keep the PostgreSQL scratch parent short for Unix sockets:

```bash
owned="$PWD/target/hosting-validation-owned"
test ! -e "$owned"
mkdir -p "$owned"
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

The [current boundary](engine-neutral-hosting.md#proposed--ordinary-engine-boundary)
and [single roadmap](sf-v1-semantic-alignment-plan.md#hosting-redesign-roadmap)
now distinguish delivered M1/bounded M2 implementation from deferred M3 and
pending final reviews. **The table and commands below are historical design
inputs only**, not current missing-deliverable or execution instructions.
They identify baseline evidence
at `b37009f8af7a7b2ed91f58f4889ed74e06013861`; they were source-inspected, not
run for the prior design/docs delivery. A passing grant/coordinator component
test could not qualify replacement engine sessions. M1 unavailable wrappers
were also not native acceptance; M2 has since converted those consumers and
executes real bodies. Current results are recorded above, not inferred here.

Before a later authorized run, use the identical package/features/profile/
expression with `cargo nextest list` instead of `run`, inspect a nonempty
intended selection, and record revision/argv/environment/outcome and limits.
Discovery can build binaries. Use task-owned output, do not remove shared
targets, and do not run destructive `just nextest-test` or the full PostgreSQL
suite as a docs check.

| Gate at historical design | Source-inspected baseline (historical layout) | Acceptance requested then; see current evidence above |
|---|---|---|
| M1 host authority/operation retention | [optional diagnostics/remote projection:173–294,394–527](https://github.com/youyuanwu/kuberic/blob/b37009f8af7a7b2ed91f58f4889ed74e06013861/kuberic-runtime/src/host/public_lifecycle_retry_tests.rs#L173-L527); [lost host observer:106–266](https://github.com/youyuanwu/kuberic/blob/b37009f8af7a7b2ed91f58f4889ed74e06013861/kuberic-runtime/src/host/service_cancellation_tests.rs#L106-L266) | Ordinary-only generic bundle without companion; compatible/incompatible action table; scope withdrawal/Abort→Close barrier and missing diagnostics; new production routing |
| M2 engine ingress/lifetime | [carrier/Remove:4407–4611](https://github.com/youyuanwu/kuberic/blob/b37009f8af7a7b2ed91f58f4889ed74e06013861/kuberic-runtime/tests/public_v1_recovery.rs#L4407-L4611); [held provider vs Close/join:351–418](https://github.com/youyuanwu/kuberic/blob/b37009f8af7a7b2ed91f58f4889ed74e06013861/kuberic-runtime/src/replicator/v1/service_adapter/tests/facade.rs#L351-L418) | Engine Active/Retired sessions without grants; delayed first handshake, lost cancel while Primary remains live, scope/effect race, real target quarantine/join and no late successor install |
| M2 public context and controller Build | [workspace provider/contract crate](https://github.com/youyuanwu/kuberic/blob/b37009f8af7a7b2ed91f58f4889ed74e06013861/kuberic-public-v1-tests/Cargo.toml), [actual V1 journeys](https://github.com/youyuanwu/kuberic/blob/b37009f8af7a7b2ed91f58f4889ed74e06013861/kuberic-level-tests/src/in_process/journeys.rs) | Same semantics through new production goal/PrepareIdle/Build route; separate voting/access, no controller-attempt carrier fields |
| M2 real independent PG | [production formation/retained transfer:1257–1570](https://github.com/youyuanwu/kuberic/blob/b37009f8af7a7b2ed91f58f4889ed74e06013861/examples/postgres/src/public_controller_tests.rs#L1257-L1570); [hosted terminal cases:205–378](https://github.com/youyuanwu/kuberic/blob/b37009f8af7a7b2ed91f58f4889ed74e06013861/examples/postgres/src/public_controller_tests.rs#L205-L378) | Ordinary-only factory; native session retirement, lost target cancel/PG helper quarantine, Close-after-Abort settlement, native replay/lineage/SQL fences through new route |
| M3 same-format restart/recovery | [session/ambiguity reopen:733–835,1476–1586](https://github.com/youyuanwu/kuberic/blob/b37009f8af7a7b2ed91f58f4889ed74e06013861/kuberic-runtime/tests/public_v1_session_renewal.rs#L733-L1586); [hot rollback-journal recovery:1730–1758](https://github.com/youyuanwu/kuberic/blob/b37009f8af7a7b2ed91f58f4889ed74e06013861/kuberic-runtime/tests/public_v1_session_renewal.rs#L1730-L1758) | Two-ended Ready revalidation after target reopen/lost reply; source/target/controller selection/effect/receipt crashes, acknowledged-prefix/PC/CC/access proof; recover matching-format state without resetting it |
| M3 clean deployment and removal | Existing [source/API/package commands](#engine-neutral-hosting), schema-rejection selection below | Replacement property coverage before each API deletion; no superseded production path remains; incompatible stores/commands/CRD intents reject before mutation; no old-state import, upgrade/downgrade or automatic data/PGDATA reset |

Historical selectors (baseline checkout only; removed owners are not available
on the current source):

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

The old coordinator was removed in M2; its pinned negatives remain a historical
property inventory, not a current selector. They used controlled effects, not actual PG/kernel cleanup. Remote-state tests
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

**Historical component/host reproduction:** names/layout in this subsection
refer to the pre-M1 evidence pins, not current ordinary production instructions.
M2 relocated these assertions through actual ordinary consumers and native
owners; use [current selections/results](#m2-accepted-evidence-and-remaining-boundaries).

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
| Native quorum vs service commit | `read_fencing::sf_d02_native_quorum_progress_is_not_a_service_commit_or_caller_cancellation_oracle` ([16–71](https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/kuberic-runtime/tests/support/read_fencing.rs#L16-L71)) |
| Receive vs service ACK | `read_fencing::sf_d02_secondary_receive_progress_is_contiguous_but_does_not_replace_service_ack` ([76–139](https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/kuberic-runtime/tests/support/read_fencing.rs#L76-L139)) |
| Acknowledged prefix | `read_fencing::sf11_acknowledged_prefix_6_through_10_survives_ambiguous_11_without_history_fabrication` ([146–214](https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/kuberic-runtime/tests/support/read_fencing.rs#L146-L214)); not the removed-engine reconciliation fixture |
| KV trait Close | `sf15_trait_close_retains_cancelled_consumer_drain` ([478–532](https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/examples/kvstore2/tests/read_fencing.rs#L478-L532)) |

For example, historical exact progress selectors (not the removed durable-scalar
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

**Historical pre-M1 selected-host qualification.** The following runs and
baseline-only commands do not assert current production callback-program support.
Current M2 ordinary lifetime/access evidence is recorded above.

Targeted runs for the host-alignment revision exercise the real selected
`ReplicaHost` and default public factory. The
[stage witnesses](https://github.com/youyuanwu/kuberic/blob/69b1cfe3c85d296463da0def3e80eaca0ddcd0a7/kuberic-runtime/tests/support/staged_access.rs)
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
At M1 these are selected-engine component witnesses entered through the testing
handle, including split-promotion observer cancellation. The legacy callback-
program RPC has a separate rejection/no-mutation witness; it is not a supported
production entry. Production ordinary-goal transport and retained response
ownership are qualified by the [M1 regressions](#ordinary-hosting-m1), not these
legacy component programs.

[Close witnesses](https://github.com/youyuanwu/kuberic/blob/aad23335c219a576d69133cee883bbd63e40b68a/kuberic-runtime/tests/public_v1_recovery.rs#L3114-L3505)
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

The removed public-host layout below is pinned historical evidence; its
component/native properties remain, while current ordinary witnesses and their
exact selectors are listed above. Baseline commands are not current API guidance.

The historical focused actual-owner and public-host vectors passed under the ordinary profile.
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
The [public vectors](https://github.com/youyuanwu/kuberic/blob/b37009f8af7a7b2ed91f58f4889ed74e06013861/kuberic-runtime/tests/public_v1_recovery/rejection_order.rs#L57-L392)
use actual C-listener configuration denial, exact metadata/status forwarding
and source retry entry as a consumption barrier. B's service-owned persistence/
ACK is withheld across real issued work and captured-boundary marked catch-up;
healthy-first completes native/application work before C is released.
A [same-native-wait handshake](https://github.com/youyuanwu/kuberic/blob/b37009f8af7a7b2ed91f58f4889ed74e06013861/kuberic-runtime/tests/public_v1_recovery.rs#L590-L625)
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
native binary oracle. Its execution wrappers launch actual nested ordinary/
native journeys; the source label does not make them source-only. Required
M2 V11A passed, while the unchanged V11B aggregate remains exit 100 with the
infrastructure/complement and later-body limits
[recorded above](#m2-accepted-evidence-and-remaining-boundaries).

## PostgreSQL

The current hosted-controller witnesses execute the independent PG native path
through production ordinary goals. Required M2 selection includes real
backup/WAL/lineage, receive plus replay and checkpoint, nonmutating replay,
native session retirement/quarantine, current-only policy/catch-up and separate
primary SQL admission. No unavailable response is positive evidence. Exact
source hashes, inventories and executed run results determine acceptance;
the historical details below do not substitute for a current execution.

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
uses production PgService, native PostgreSQL processes/RPCs and the real
ordinary Reconciler. It does not install host transfer grants or manually
advance target role/configuration to break admission ordering.
The `_p6` filename identifies existing tests, not today's roadmap cursor.
Evidence distinguishes live-status drain, idle/slot retirement, orderly
terminal progress, marked-successor WriteQuorum rejection, ordinary unmarked
WriteQuorum/All success and retained Abort. See
[current PG semantics](classic-v1-runtime.md#postgresql-custom-path) and
[audit dispositions](sf-v1-port-ledger.md#current-semantic-and-gap-dispositions).
Do not infer native differential, arbitrary survivor election, WAL-growth SLA
or power-loss proof from these process/file observations.

### Custom-native controller reproduction and limits

The commands remain the authorized current smoke entry points. The current
required witnesses include:

- `m2_pg_ordinary_fresh_admission` and `m2_pg_native_copy_boundaries`;
- `m2_pg_joint_to_current_retains_native_session_while_pending`, including
  actual WAL replay and WriteQuorum before the held source receipt, a held
  target current-role receipt and a worker that misses the joint publication;
- `m2_pg_already_live_joint_receiver_survives_pending_current`, capturing an
  already-running joint Peer receiver before current-only installation and
  checking actual native receiver/sender, gateway-task, slot and ACK continuity;
- `m2_pg_incompatible_or_retired_session_cannot_continue_while_pending`,
  delayed first establishment, lost target cancellation and retained actual joins;
- the `m2_pg_live_publication_*` cases, changing
  newest publication identity/configuration facts through test discovery faults
  while preserving the actual live session ID and owner, with native fencing,
  absent quorum/admission credit and a held real driver join for each fault;
- `m2_pg_membership_registration_retirement_orders_and_sticky_join`, covering
  both registration/retirement orderings and held/lost actual membership joins;
- exact-session cache/ACK/slot fencing and matching-format acknowledged-data
  reopen without reset.

Invalidation witnesses await the released retained ordinary Build and native
owner before teardown. Source Primary, Remove success and local slot retirement
are not target containment or stale-completion evidence.

The PG-N catalog keeps these in routine smoke along with the enabled native
regressions. Longer direct/native failover, switchover and replacement bodies
remain diagnostics; they do not establish full M3 controller policy. Ordinary
stable-secondary SQL is still denied by the generic host. Administrative
standby reads establish native data/replay only, not client access.

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

### Historical pre-M1 controller/component evidence

The following paragraphs retain the earlier evidence boundary. They describe
historical source owners and observations, not a passed run of today's source.
Current results must name the live witness and its actual execution.

The historical controller witness observes successful fresh Open and explicit
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

The separate historical
[external runtime contract fixture](https://github.com/youyuanwu/kuberic/blob/ce9df4ad85603b935752ed7184d9314a778b6eb5/kuberic-runtime/tests/support/custom_control.rs)
uses an independently attached engine, real host journals/control dispatch and
owned child tasks, but deliberately has no native database/quorum implementation.
Its truthful progress-unavailable and unsupported wait results must not be
called a native positive witness:

Historical baseline-only commands:

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

**Experimental development assets, not release installation.** M1 had no KinD
acceptance. M2's full matching-image bootstrap passed on the exact recovery
source recorded [above](#m2-accepted-evidence-and-remaining-boundaries), including
secondary reconstruction and final write/read. It predates the last controller
repair. Replacement/failover are separate unqualified diagnostics; do not infer
their success or a final-source image pass. Future authorized reproduction
requires an explicitly owned disposable cluster and matching runtime-only
binaries/libraries/images, not implicit rebuilding from unrelated cache:

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
just level-triggered-kind-test bootstrap
just delete-kind-cluster
```

The [ownership recipes:138–210](../../../justfile#L138-L210) require an explicit
receipt and reject default/shared cluster coordinates. The pre-M1 bootstrap/replacement/
failover scenarios inspected real deployment resources, authority, data and stale-writer
fencing, with sustained containerd pause/resume in failover; see
[live tests](../../../kuberic-level-tests/tests).
Do not run this tier to validate a Markdown edit or confuse it with native SF
hosted differential, deployment-security certification or production readiness.

[public-context]: https://github.com/youyuanwu/kuberic/blob/45749d3744b43095be9bc9ec3fb4abf20633a537/scripts/fixtures/classic-v1-provider/tests/public_copy_context.rs#L377-L647
[public-context-ksf]: https://github.com/youyuanwu/kuberic/blob/45749d3744b43095be9bc9ec3fb4abf20633a537/kuberic-runtime/src/testing/public_v1_conformance.rs#L905-L918
[public-context-ci]: ../../../.github/workflows/CI.yml
