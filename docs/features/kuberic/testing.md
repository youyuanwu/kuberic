# Kuberic Test Strategy

Kuberic validation is split into repository-wide nextest tiers, doctests and
explicitly owned Kubernetes scenarios. The removed classic v1 stack has no
remaining test selection.

## Workspace Validation

```bash
just install-nextest
cargo check --workspace
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --doc --workspace --all-features
just nextest-test
```

The `ordinary` profile is cluster-free and excludes PostgreSQL, DEX's real
Kubernetes target and build-only guards. `just nextest-test` runs it with two
test threads in a clean non-incremental `target/nextest-ordinary`; the SQLite
group remains serial and process-boundary grouping prevents competing recovery
fixtures. CI divides the current ordinary inventory into four slices.

The all-features archive includes every test binary, including resource-backed
tiers. Validate its exact-one disposition and partition union before relying on
it:

```bash
just nextest-archive
just nextest-validate-archive
```

The consolidated inventory accounts for 1,388 tests: 1,134 ordinary, one
build-only, 213 directly runnable PostgreSQL tests, 19 live KinD tests covering
12 scenario identities, one DEX live test and 20 mapped parent-driven
subprocess helpers.
Use its generated counts as the inventory evolves.

The ignored runtime `public_api_privacy` binary belongs only to the `build-only`
tier (`just nextest-list build-only`). It invokes Cargo and needs dependency
caches and workspace sources, so CI runs `scripts/check_runtime_public_api.sh`
in the build-artifact job before archiving, never on archive-only runners.
`just nextest-build-only` runs the same guard, and `just nextest-archive`
includes it as a prerequisite. Archive inventory validation still accounts for
the ignored test exactly once.

Internal lifecycle dependency isolation uses stable compiler and Clippy policy
plus a focused ordinary source test:

```bash
cargo test -p kuberic-runtime --test lifecycle_capability_boundaries
```

Rust visibility lints reject unreachable public declarations and private
interface mismatches. Runtime-local Clippy restrictions keep broad runtime and
direct-primary types out of narrow host modules. The source test covers the
remaining Kuberic-specific relationships: aggregate capability traits and
owners, guarded aliases, trait/view escape paths, fixture gating,
construction-only wiring, recursive modules, and the single admitted-build
coordinator. It does not snapshot every private method, field or consumer.
These checks complement behavior suites and the external API/privacy guard;
they do not replace either.

The syntax guard is deliberately bounded rather than a whole-program Rust
analyzer. New explicit Clippy suppressions, macro-generated lifecycle wiring,
block-local capability trait graphs, reverse conversions, and transport child
modules require direct review instead of extending the guard with another
source-language model.

## Level-Triggered Unit and Durable Validation

Use `just nextest-test` for the complete cluster-free tier. For focused
debugging, nextest accepts Cargo package/target selectors in addition to the
repository profile:

```bash
cargo nextest run --profile ordinary -p kuberic-runtime --all-features
cargo nextest run --profile ordinary -p kuberic-controller
```

The runtime `host::tests::crash_boundaries` and `host::tests::recovery` parent
tests execute their ignored child helpers and
reopen durable stores. Top-level ignored process entries are not omitted
coverage.

Controller library tests verify that the checked-in CRD equals the generated
schema. Runtime protocol tests guard representative status growth and reject quadratic
evidence expansion.

## Service Fabric Public V1 Phase 6 Preview

Phase 6 adds a dormant, built-in stateless preview on top of the existing
Public V1 operation/evidence gates. It is selectable only through
test/development features. Ordinary production construction and manifests
remain on protocol 9/schema 6 and the legacy managed engine; the preview uses
fresh protocol 11/schema 11 state.

Run exact hosted selectors serially. Structural KSF checks do not start nested
Cargo work; the separate ignored KSF execution gate does. Hosted runtime and
provider journeys own real listeners and durable roots. `-j 1` is the
reproducible focused boundary.

### Runtime ownership, hosted recovery and conformance

The inventory is exactly 20 direct recovery tests, two ordinary structural
catalog tests and one ignored bounded execution gate:

```bash
scripts/assert_nextest_selection.sh \
  --label phase6-runtime \
  --require-binary stateless_preview_recovery \
  --require-binary public_v1_conformance \
  --require-test preview_open_owns_registration_listener_and_writer_exactly_once \
  --require-test hosted_preview_supersession_cancels_build_root_and_drains_descendant_before_close \
  --require-test hosted_preview_contained_close_restarts_without_replaying_interrupted_children \
  --require-test hosted_preview_role_address_publishes_replaces_and_clears \
  --require-test primary_write_revocation_fences_provider_apply_and_peer_ack_interleavings \
  --require-test bounded_concurrent_peer_work_ignores_hung_minority_and_drains_on_close \
  --require-test hosted_preview_rebuild_retires_and_restarts_fresh_replacement \
  --require-test ambiguous_suffix_exactly_replays_quorum_peers_despite_higher_advertised_progress \
  --require-test public_v1_conformance_oracle_has_exactly_ksf_01_through_ksf_16 \
  --require-test built_in_preview_ksf_mappings_require_each_authorized_observation -- \
  cargo nextest list -p kuberic-runtime --all-features \
    --profile ordinary \
    -E 'binary(stateless_preview_recovery) or binary(public_v1_conformance)'

cargo nextest run -j 1 -p kuberic-runtime --all-features \
  --profile ordinary -E 'binary(stateless_preview_recovery)'

cargo nextest run -j 1 \
  -p kuberic-runtime --all-features --profile ordinary \
  -E 'test(public_v1_conformance_oracle_has_exactly_ksf_01_through_ksf_16) or test(built_in_preview_ksf_mappings_require_each_authorized_observation)'

cargo nextest run -j 1 -p kuberic-runtime --all-features \
  --profile ordinary --run-ignored only \
  -E 'test(public_v1_conformance_executes_strict_and_hosted_preview_scenarios)'
```

The direct binary covers immutable selection, exact engine/provider-writer/
listener ownership, schema-11 bootstrap, no-fallback startup, static
reconstruction, durable-before-ACK writes, write-authority interleavings,
exact-current-primary ingress, bounded concurrent peer work, ambiguous suffix
containment, higher-epoch session renewal, failover/switchover, build, removal,
data loss, permanent retirement, empty replacement and post-replacement
restart. The
full hosted switchover and build chains execute in this
`stateless_preview_recovery` binary because it owns both the preview runtime
fixtures and the feature-gated controller adapter.

The shared catalog contains exactly KSF-01 through KSF-16. The
`built-in-preview` subject has public-observable scenarios for every KSF-01
through KSF-15 row. KSF-09 and KSF-10 have positive preview coverage but
retain negative replacement-artifact guards while the legacy managed
capability/listener exists. KSF-16 has no preview scenario and remains the
Phase 7 legacy-continuation guard.

### Controller operation/evidence boundaries

The controller selection is exactly the 10 `public_v1_evidence` and six
`public_v1_session_renewal` tests:

```bash
scripts/assert_nextest_selection.sh \
  --label phase6-controller \
  --require-binary public_v1_evidence \
  --require-binary public_v1_session_renewal \
  --require-test wrong_operation_target_is_fenced_before_dispatch_for_every_mutating_program \
  --require-test hosted_preview_removal_drains_build_excludes_peer_and_rejects_stale_session \
  --require-test hosted_preview_unreachable_survivor_blocks_access_until_return_or_retirement -- \
  cargo nextest list -p kuberic-controller --all-features \
    --profile ordinary \
    -E 'binary(public_v1_evidence) or binary(public_v1_session_renewal)'

cargo nextest run -j 1 -p kuberic-controller --all-features \
  --profile ordinary \
  -E 'binary(public_v1_evidence) or binary(public_v1_session_renewal)'
```

The feature-gated hosted adapter implements history, session-renewal,
`PublicV1OperationApi` and evidence APIs. This selector covers operation
target fencing, evidence acceptance/removal/replacement behavior and
session-renewal planning/order. The full accepted evaluator authority ->
executor -> `PublicV1Operation` command -> preview runtime owner -> matching
durable report -> effect chain is exercised by the runtime
`stateless_preview_recovery` selector, not by these controller-only fixtures.
Wrong targets, predecessor reports, changed authority and stale resource
versions fail before effect.

### Real SQLite and KVStore2 recovery journeys

These are three exact tests per real provider:

```bash
scripts/assert_nextest_selection.sh \
  --label phase6-providers \
  --require-binary durable_data_loss \
  --require-binary public_v1_preview \
  --require-test sqlite_fresh_restart_build_and_corrective_empty_replacement \
  --require-test kvstore2_fresh_restart_build_and_corrective_empty_replacement \
  --require-test sqlite_real_provider_restart_settlement_and_handoff \
  --require-test kvstore2_real_provider_restart_settlement_and_handoff \
  --require-test sqlite_original_primary_loss_promotes_survivor_and_fences_return \
  --require-test kvstore2_original_primary_loss_promotes_survivor_and_fences_return -- \
  cargo nextest list -p sqlite-replicated -p kvstore2 --all-features \
    --profile ordinary \
    -E 'test(sqlite_fresh_restart_build_and_corrective_empty_replacement) or test(kvstore2_fresh_restart_build_and_corrective_empty_replacement) or test(sqlite_real_provider_restart_settlement_and_handoff) or test(kvstore2_real_provider_restart_settlement_and_handoff) or test(sqlite_original_primary_loss_promotes_survivor_and_fences_return) or test(kvstore2_original_primary_loss_promotes_survivor_and_fences_return)'

cargo nextest run -j 1 -p sqlite-replicated -p kvstore2 \
  --all-features --profile ordinary \
  -E 'test(sqlite_fresh_restart_build_and_corrective_empty_replacement) or test(kvstore2_fresh_restart_build_and_corrective_empty_replacement) or test(sqlite_real_provider_restart_settlement_and_handoff) or test(kvstore2_real_provider_restart_settlement_and_handoff) or test(sqlite_original_primary_loss_promotes_survivor_and_fences_return) or test(kvstore2_original_primary_loss_promotes_survivor_and_fences_return)'
```

The corrective-replacement journeys freeze a copy snapshot, commit a known
retained operation, withhold admission until the target durably closes that
gap, retire a divergent higher-progress incarnation, build an empty
replacement and restart it without recovering retired bytes.

The composite journeys prepare identical real-provider
`applied_lsn > committed_lsn` suffixes, restart every live preview process,
prove access remains closed until exact write-quorum settlement, and then
verify one writable primary and independently known data in both providers.
They subsequently hand primary authority to the former secondary, reject the
old primary's write and commit a known post-handoff write through the new
primary.

The separate primary-loss journey stops the original primary after the
survivor has exact committed data, promotes that survivor through a public
`FailoverPromotion`, verifies one writer and the known data, then restarts the
predecessor and proves it remains access-closed.

### Timeouts and implementation evidence

Every newly added Phase 6 test has an internal timeout no greater than 270
seconds:

- direct runtime tests: 30, 120, 150 or 180 seconds;
- exact structural mapping override: 270 seconds;
- ignored nested KSF execution gate: one absolute 270-second parent deadline
  shared by strict-custom and built-in-preview inventory/execution, with
  process-tree termination/reaping regression coverage;
- new controller target/removal/survivor tests: 270 seconds;
- SQLite/KVStore2 journeys: 120 or 180 seconds, with tighter build waits;
- terminal inbound receipt reopen/replay: an exact 270-second nextest override,
  proven by an exact one-test selector.

The nextest 300-second termination limit is an external backstop and is not
used to satisfy the Phase 6 requirement. A retained one-shot notification
prevents the hosted switchover commit handshake from being lost during nested
conformance execution. Focused serial selectors remain the supported way to
separate protocol failures from transient build/host contention.

The third-final-review corrections retain 23 direct recovery scenarios while
adding exact controller retry cuts, synchronous inbound supersession,
failover evidence reconciliation, immediate local-source fail-closed behavior
and one primary-loss journey per real provider. Final evidence is recorded in
the Phase 6 PAW `Docs.md`.

- direct runtime recovery: 23/23 passed in 68.404s;
- structural KSF catalog: 2/2 passed; the separate ignored exact conformance
  execution gate passed in 133.111s inside its 270-second parent bound;
- controller operation/evidence and renewal: 16/16 passed in 0.074s;
- real provider replacement/restart, restart/handoff and primary-loss
  journeys: 6/6 passed in 13.062s on the final focused serial rerun;
- `just nextest-test`: passed. The recipe used a clean
  `target/nextest-ordinary`, `CARGO_INCREMENTAL=0`, disabled test debuginfo and
  two test threads; no nested conformance process ran in the ordinary tier;
- bounded PostgreSQL smoke: after one contained acknowledged-row failover
  timeout at the unchanged 240-second bound, the immediate unchanged rerun
  passed 4/4 Public V1 scenarios in 32.091s and 8/8 representative scenarios
  in 156.548s;
- lifecycle capability boundaries passed 6/6; runtime API inventory, workspace
  all-feature check, formatting and diff hygiene passed.

The KSF runner and real-provider journeys have deliberately bounded but finite
headroom on this host. Run exact serial selectors before attributing a timeout
to protocol failure.

The first PostgreSQL smoke immediately after a clean rebuild hit the existing
240-second internal bounds in the acknowledged-row failover and planned
switchover scenarios. Their timeout containment reaped all PostgreSQL
processes; an immediate unchanged rerun passed 4/4 plus 8/8. No deadline was
increased and no result from the timed-out run is claimed as passing evidence.

The first PostgreSQL documentation attempt exhausted `/data` while compiling;
removing only `target/debug/incremental` restored 15 GiB. A second attempt used
a project-local path that was too long for PostgreSQL's 107-byte Unix socket
limit. The exact short project-local scratch command in the PostgreSQL section
below then passed. These are known environment/contention limits, not accepted
test skips or retries.

The full `just nextest-postgres` suite is not part of routine Phase 6
verification and is not claimed unless explicitly requested.

## Level-Triggered Live Validation

Live scenarios require Docker, KinD, kubectl, Just, the pinned Rust toolchain,
the pinned nextest binary and `protoc`. The `kind` profile and `kind-live`
group select 19 ignored live tests covering 12 scenario identities and use one
execution slot. The `scale-down` identity is split into two bounded tests, and
the scale-down and scale-up adversarial identities are each split into four
fault campaigns, so no executable combines more than five minutes of live
work.
Use a nondefault cluster, repository-local kubeconfig and exact matching
context:

```bash
export KIND_CLUSTER_NAME=kuberic-level-dev
mkdir -p target/kuberic-level-dev
export KUBECONFIG="$PWD/target/kuberic-level-dev/kubeconfig"
export KUBE_CONTEXT="kind-${KIND_CLUSTER_NAME}"
export KUBERIC_AGENT_BEARER_TOKEN=local-level-triggered-token

just create-kind-cluster
just level-triggered-images
just level-triggered-install
just level-triggered-kind-test bootstrap replacement failover
just level-triggered-kind-test switchover scale-down scale-up
just delete-kind-cluster
```

`all` expands to replacement, quorum-loss, adversarial switchover,
scale-down and scale-up scenarios. Run it on fresh bootstrap; standalone
failover requires another fresh cluster. `scale-up-full` expands to healthy,
multi-add and adversarial scale-up.

On failure, run `just level-triggered-diagnostics` before deleting the owned
cluster. Every mutation and deletion verifies the ownership receipt and exact
context.

The live copy gate is injected only by the owned-cluster installation recipe.
It is absent from the checked-in user sample and is not a supported production
API.

## SQLite V2 Unit and In-Process Validation

SQLite uses actual WAL frames, public stateful application interfaces, durable
agent/application stores and in-process transport. It requires no Kubernetes
API, container runtime or external database process.

```bash
cargo test -p sqlite-replicated \
  --all-features -- --test-threads=1
cargo clippy -p kuberic-runtime -p sqlite-replicated \
  --all-targets --all-features -- -D warnings
```

The ordinary nextest tier includes the SQLite example and its private barrier
module's tests under one serial
`sqlite` group, so `just nextest-test` preserves the same resource boundary.

Coverage includes bootstrap, replacement, failover, planned switchover,
sequential scale-up, secondary scale-down, quorum restoration, copy restart and
authority races. Fresh v2 storage is required; no classic data import exists.

See the [SQLite design](../sqlite/design.md).

## PostgreSQL V2 Host-Local Validation

PostgreSQL tests run real local subprocesses with durable stores and
exact-session authority. They require an unprivileged Linux account, pidfds,
subreapers, readable `/proc` and compatible PostgreSQL binaries. PostgreSQL 16
is the validated host major. Missing prerequisites fail rather than skip.
The `just` PostgreSQL recipes detect WSL and skip the tier because its kernel
does not provide the pidfd reaping semantics required by the process supervisor.

```bash
just nextest-postgres-smoke-inventory
just nextest-postgres-smoke
just nextest-postgres
# Same deterministic four-way hash partition used by an isolated CI runner:
just nextest-postgres 1/4
cargo clippy -p postgres-replicated --all-targets --all-features -- -D warnings
```

The inventory target proves all four exact PostgreSQL Public V1 scenario
tests are selected before any PostgreSQL process starts. The scenarios cover
primary lifecycle/joint configuration, build cancellation/retirement,
data-loss history/replay and contained close, and each records the applicable
KSF rows. The smoke target depends on that inventory and covers representative
lifecycle paths. It runs the four Public V1 scenarios package-only before a
separate eight-test workspace pass so compilation I/O cannot consume their
runtime budget. Their data roots use `KUBERIC_POSTGRES_PUBLIC_V1_TMPDIR`, or
the smoke recipe selects sufficiently sized `/dev/shm` before falling back to
`/tmp`. CI-derived nextest profiles terminate an individual test at five
minutes. Run the full target only when explicitly requested or in the dedicated
CI validation that owns its resource budget.

To keep all scratch state in the repository, use a short path so PostgreSQL's
107-byte Unix-domain socket limit is not exceeded:

```bash
mkdir -p "$PWD/p"
TMPDIR="$PWD/p" KUBERIC_POSTGRES_PUBLIC_V1_TMPDIR="$PWD/p" \
  just nextest-postgres-smoke
rmdir "$PWD/p"
```

The matrix covers physical build/rewind, fencing, failover, switchover,
replacement, scaling, quorum restoration, read-only secondaries and
application/agent restart. The profile has one execution slot. Its complete
215-test inventory is 213 directly partitioned tests plus two ignored
subprocess helpers executed by mapped parent tests. PostgreSQL has no
application-specific KinD test.

See the [PostgreSQL design](../postgres/design.md).

## DEX Validation

DEX default tests are cluster-free:

```bash
CARGO_BUILD_JOBS=2 cargo test -p kuberic-dex --all-targets
CARGO_BUILD_JOBS=2 cargo test -p kuberic-dex --doc
CARGO_BUILD_JOBS=2 cargo clippy -p kuberic-dex \
  --all-targets -- -D warnings
```

The mocked provider and real API targets are:

```bash
CARGO_BUILD_JOBS=2 cargo test -p kuberic-dex \
  --features kubernetes --test kubernetes_checkpoint -- --nocapture
CARGO_BUILD_JOBS=2 cargo test -p kuberic-dex \
  --features kubernetes --test kubernetes_checkpoint_real
```

The real target requires explicit nondefault `KIND_CLUSTER_NAME`,
`KUBECONFIG`, and matching `KUBE_CONTEXT`. It performs access review and owns
its temporary namespace lifecycle. `just nextest-list dex-live` selects exactly
that target. Pull requests run it on the owned smoke cluster; main, version-tag
and CI-manual events run it on a separate receipt-gated cluster.

## Distribution Boundary

No current workflow publishes Kuberic images. Controller/KVStore2 Dockerfiles
and installation recipes are experimental local/CI assets. Existing external
classic images, if present, are unsupported historical artifacts.
