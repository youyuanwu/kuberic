# Durable Remove Replica Implementation Plan

## Overview

Port remove-replica as the second explicitly selected, default-off workflow on
the durable-execution kernel while preserving the existing explicit path as the
default and semantic baseline. The implementation will reuse the existing
typed-activity, fused-progression, prepared-effect, checkpoint, admission, and
terminal-compaction contracts. Switchover-specific operator hosting will be
generalized only where both workflows need the same capability.

The work is deliberately sequenced to make amortization attributable. Phase 1
freezes the pre-port shared baseline and introduces production-only legacy
measurement boundaries before shared code changes. Later phases assign every
new line either to the second workflow body, second-workflow integration, or
the same shared scope whose pre-port value was frozen. The documentation phase
runs only after the completed implementation produces the actual complexity,
checkpoint, and feasibility results.

Planning mode: single-model using `gpt-5.6-sol`.

## Current State Analysis

- The explicit remove-replica workflow is selected from healthy scale-down and
  stale/missing-secondary recovery paths, persists its operation in CRD status,
  and advances through `decide_remove_replica`
  (`kuberic-operator/src/reconciler.rs:771-816`,
  `kuberic-operator/src/reconciler.rs:1028-1079`,
  `kuberic-operator/src/reconciler.rs:1280-1329`,
  `kuberic-operator/src/reconciler.rs:1477-1518`,
  `kuberic-operator/src/durable/remove_replica.rs:51-229`).
- Its irreversible ordering is reduced-Current commit evidence, cleanup,
  stable-snapshot publication, then completion
  (`kuberic-operator/src/durable/remove_replica.rs:766-802`,
  `kuberic-operator/src/durable/remove_replica.rs:805-1017`).
- The kernel already provides workflow-neutral typed activities, prepared
  exposure, result reservation, fused observe-and-progress, conservative CAS
  handling, quarantine, and terminal compaction
  (`durable-execution/src/typed.rs:5-181`,
  `durable-execution/src/host.rs:308-449`,
  `durable-execution/src/host.rs:538-686`,
  `durable-execution/src/checkpoint.rs:133-370`).
- The operator host above the kernel is still named and typed for switchover:
  feature selection, status reference, workflow/activity/terminal types,
  checkpoint measurements, recovery logic, and reconciler routing
  (`kuberic-operator/Cargo.toml:11-18`,
  `kuberic-operator/src/crd.rs:43-55`,
  `kuberic-operator/src/crd.rs:118-127`,
  `kuberic-operator/src/durable/pilot.rs:47-410`,
  `kuberic-operator/src/durable/pilot_store.rs:18-381`,
  `kuberic-operator/src/reconciler.rs:2633-3273`).
- Shared operator effects already preserve exact correlated replica commands,
  exact-UID label commands, portable outcomes, and conservative failure
  classification, but the pilot whitelist and bridge do not cover exact-UID
  deletion or remove-replica terminal publication
  (`kuberic-operator/src/durable/effects.rs:38-163`,
  `kuberic-operator/src/durable/effects.rs:262-289`,
  `kuberic-operator/src/durable/effects.rs:589-811`).
- The current complexity script reports overlap-checked switchover scopes and
  the current shared scope of 1,208 executable lines / 110 decision points, but
  it has neither a production-only explicit remove scope nor provenance for
  `shared_before`
  (`scripts/measure-switchover-complexity.py:13-181`,
  `.paw/work/durable-remove-replica/CodeResearch.md:638-706`).
- The feasibility test scans the complete crate manifest for `tokio`, although
  the runtime is present only in development dependencies
  (`durable-execution/tests/feasibility.rs:174-181`,
  `durable-execution/Cargo.toml:16-29`).

## Desired End State

- `remove_replica_execution_mode` defaults to explicit and offers an opt-in
  durable-pilot selection only when a dedicated default-off Cargo feature is
  compiled.
- The kernel-hosted workflow accepts the same frozen remove operation and
  reaches outcomes equivalent to explicit completion, proven-safe compensated
  failure, or typed unsafe ambiguity.
- Exact replica, label, and deletion effects are accepted before dispatch,
  correlated with authoritative observations, and never redriven from an
  unknown outcome without reload.
- Commit evidence is captured inside durable workflow state before cleanup;
  terminal checkpoint acceptance precedes final reduced-topology and operation
  status publication.
- The switchover pilot continues to pass unchanged behavioral tests through the
  generalized shared host.
- Complexity output contains reproducible `shared_before`, `shared_after`,
  `shared_growth`, `legacy_remove`, `remove_body`, `remove_integration`,
  `remove_marginal`, and classification values for executable lines and
  decision points, with overlap rejection across all charged scopes.
- Three no-fault three-member ScaleDown samples report effect/observation
  counts, durable boundaries, accepted writes, and active/terminal checkpoint
  byte ranges against the stable ceilings.
- The feasibility classifier is recomputed using a library-dependency-aware
  runtime-neutrality predicate, and README/roadmap text reflects the actual
  result and actual amortization measurements.

## Architectural Decisions

### Shared host boundary

Use one operator-level durable host vocabulary for both workflows: workflow
identity, generic measured checkpoint storage, portable prepared effect
commands/outcomes, dispatch-permit consumption, checkpoint recovery, and
terminal accounting. Keep each workflow's orchestration state, typed activity
payload, admission rules, and terminal payload in its own module. This keeps
workflow logic attributable while charging genuinely reused hosting once.

Alternative rejected: copy `pilot.rs` and `pilot_store.rs` for remove-replica.
That would duplicate already reusable behavior, inflate marginal cost, and fail
FR-011 even if the workflow body itself were small.

### Workflow semantics

Use the existing explicit remove-replica operation and decision function as the
semantic oracle. The kernel workflow repeatedly obtains authoritative
observations, applies the existing decision contract, folds deterministic
state-only transitions without separate external dispatch, and emits prepared
external commands for replica-agent operations, exact-UID role mutation, and
exact-UID Pod deletion. Final reduced topology publication is represented in
the terminal payload and applied only after terminal checkpoint acceptance.

Alternative rejected: reimplement the remove protocol as a new independent
state machine. That would make safety parity harder to prove and create a
second protocol definition rather than measuring framework amortization.

### Feature and runtime selection

Add a dedicated `durable-remove-replica-pilot` feature and a remove-specific
execution-mode field that defaults to `Explicit`. Use a separate durable
execution reference in status so explicit and kernel state cannot be confused.
Unsupported durable selection returns an error without fallback.

### Complexity attribution

Freeze `shared_before` as named baseline constants tied to the pre-port commit
and assert in Phase 1 that the current shared ranges equal those constants.
After implementation, measure the same logical shared labels as
`shared_after`; their delta is the only shared growth charged to
`remove_marginal`. Mark the production-only portion of explicit
`remove_replica.rs`, the new remove workflow body, and remove-specific
integration ranges separately. Nested diagnostic ranges remain non-additive.

This avoids deriving `shared_before` from an already modified checkout and
prevents generalized code from being reclassified after seeing the result.

## What We're NOT Doing

- Removing, replacing, or changing the default explicit remove-replica path.
- Enabling durable switchover or durable remove-replica by default.
- Porting add-replica, failover, create-partition, or election workflows.
- Redesigning ReplicaAgent behavior or the gRPC protocol.
- Relaxing current quorum, UID, incarnation, generation, epoch, protocol,
  configuration, admission, checkpoint, or terminal payload bounds.
- Claiming broad durable-framework graduation from only two workflows.
- Adjusting measurement scopes or thresholds after seeing the final numbers.
- Modifying local KinD deployments to mask an environment-stale integration
  test.

## Phase Status

- [x] **Phase 1: Freeze Attributable Complexity Baselines** - Established immutable pre-port shared and explicit-remove accounting before shared code changes.
- [x] **Phase 2: Generalize the Shared Operator Host** - Generalized hosting, effects, measurements, and terminal accounting while preserving switchover behavior.
- [ ] **Phase 3: Implement the Kernel Remove-Replica Workflow** - Add typed orchestration, admission, replay, prepared effects, terminal outcomes, and focused fault coverage.
- [ ] **Phase 4: Integrate Operator Routing and Produce Measurements** - Add feature-gated CRD/reconciler selection, end-to-end coverage, and final amortization/checkpoint evidence.
- [ ] **Phase 5: Correct Feasibility Evidence and Documentation** - Fix runtime-neutrality classification and update README, roadmap, testing docs, and as-built documentation from measured results.

## Phase Candidates

None.

---

## Phase 1: Freeze Attributable Complexity Baselines

### Objective

Make pre-port and post-port attribution mechanically reproducible before any
shared infrastructure is changed.

### Changes Required

- **`kuberic-operator/src/durable/remove_replica.rs`**
  - Add complexity boundary markers around production implementation only,
    ending before the in-file test module currently beginning near line 1665.
  - Do not alter decision behavior in this phase.
- **`scripts/measure-switchover-complexity.py`**
  - Generalize the report title/output vocabulary from switchover-only to
    durable workflow complexity while preserving existing labels for consumers.
  - Add `legacy_remove` using the new production-only marker.
  - Record `shared_before` constants for both metrics together with the exact
    pre-port source revision `8d773ef2b32fd3073e11849a131fe2c2f5e6b97b`
    and the constituent shared labels.
  - Assert that the Phase 1 current shared measurement equals the frozen
    baseline, preventing a stale or manually invented constant.
  - Add reusable calculation/reporting support for `shared_after`,
    `shared_growth`, marginal ratios, and Spec classification thresholds.
    Before later scopes exist, report second-workflow fields as unavailable
    rather than assigning overlapping or fabricated ranges.
  - Preserve missing/reversed marker checks and strengthen charged-scope
    validation to reject duplicate labels, duplicate segments, and all
    cross-category line overlap.
- **New `scripts/tests/test_measure_switchover_complexity.py`**
  - Use Python's standard-library `unittest` only.
  - Exercise missing/reversed markers, duplicate labels, duplicate files,
    duplicate segments, intersecting ranges, baseline equality, ratio
    calculation, and positive/negative/inconclusive classification.

### Attribution Checkpoint

Record in the phase review:

- `shared_before = 1,208 executable lines / 110 decision points`;
- exact constituent labels and marker ranges;
- measured production-only `legacy_remove`;
- the baseline commit and successful equality assertion.

No Phase 2 change begins until this checkpoint is reproducible.

### Success Criteria

#### Automated Verification

- [ ] `python3 scripts/measure-switchover-complexity.py`
- [ ] `python3 -m unittest scripts.tests.test_measure_switchover_complexity`
- [ ] Output retains the existing switchover measurements.
- [ ] Output reports the production-only `legacy_remove` scope.
- [ ] The script fails when a fixture or temporary in-memory segment registry
  contains overlap, duplicate segments, missing markers, or reversed markers.
- [ ] The script proves current shared measurements equal the frozen
  `shared_before` constants.
- [ ] `cargo fmt --all -- --check`

#### Manual Verification

- [ ] Every charged source line belongs to exactly one additive category.
- [ ] `legacy_remove` excludes in-file tests.
- [ ] The baseline revision and values are printed clearly enough to reproduce
  the later `shared_growth` calculation.

---

## Phase 2: Generalize the Shared Operator Host

### Objective

Extract or generalize only the operator-hosting capabilities required by both
switchover and remove-replica, with no routing or behavior change yet.

### Changes Required

- **`kuberic-operator/src/durable/mod.rs`**
  - Expose workflow-neutral host/effect/store interfaces under the existing
    switchover feature in this phase, without referring to a not-yet-declared
    remove feature.
  - Structure the `cfg` expression so Phase 3 can add the remove feature as a
    second enabling condition without changing the shared interface.
- **`kuberic-operator/src/durable/pilot.rs`**
  - Move generic checkpoint-store type aliases, execution identifiers,
    prepared-effect envelopes, dispatch-permit integration, and reusable
    terminal-accounting interfaces into clearly charged shared boundaries.
  - Retain switchover state, activity payloads, resolver rules, admission
    projection, and terminal transcript in the switchover module.
  - Keep compatibility names or narrow adapters where needed to avoid changing
    existing tests and call sites solely for renaming.
- **`kuberic-operator/src/durable/pilot_store.rs`**
  - Generalize measurement classification through a workflow-provided decoder
    or trait that identifies passive observations, external effects, completed
    activities, and terminal accounting without importing switchover activity
    types.
  - Rename log/event vocabulary to durable workflow/execution terminology while
    retaining all attempt-versus-accepted-write distinctions.
  - Keep Kubernetes and in-memory provider behavior unchanged.
- **`kuberic-operator/src/durable/effects.rs`**
  - Generalize action validation and quarantine recovery entry points so each
    workflow supplies its allowed command kinds and postcondition validation.
  - Preserve exact command serialization, signature validation, single-use
    permit consumption, proven-no-admission handling, and unknown-outcome
    classification.
- **`kuberic-operator/src/reconciler.rs`**
  - Isolate the generic effect-dispatch bridge and common host outcome handling
    inside shared complexity boundaries.
  - Keep switchover routing and terminal publication behavior unchanged.
- **Tests in `pilot.rs`, `pilot_store.rs`, and `effects.rs`**
  - Preserve all existing switchover replay, exposure, quarantine, checkpoint,
    terminal-accounting, and command-adapter coverage.
  - Add shared-host tests for workflow-provided activity/terminal decoding
    without enabling remove routing.
- **`scripts/measure-switchover-complexity.py`**
  - Extend the same shared labels to include genuinely common extracted ranges.
  - Introduce separate switchover-specific integration labels where code leaves
    the shared boundary.
  - Print interim `shared_after` and `shared_growth`; do not yet classify final
    remove amortization.

### Success Criteria

#### Automated Verification

- [ ] `cargo test -p kuberic-operator --features durable-switchover-pilot`
- [ ] `CARGO_BUILD_JOBS=2 CARGO_INCREMENTAL=0 RUST_MIN_STACK=4194304 cargo test -p kvstore --test reconciler test_durable_execution_switchover_pilot_`
- [ ] `cargo test -p kuberic-durable-execution`
- [ ] `cargo test -p kuberic-durable-execution --features kubernetes`
- [ ] `python3 scripts/measure-switchover-complexity.py`
- [ ] `cargo check --all-targets`
- [ ] `cargo build --all-targets`
- [ ] `cargo clippy --all-targets --all-features -- -D warnings`
- [ ] `cargo fmt --all -- --check`

#### Manual Verification

- [ ] Switchover's prepared commands, terminal transcript, and reconciliation
  output remain byte/behavior compatible except for non-contractual telemetry
  naming.
- [ ] Every generalized range is exercised by the switchover host and exposes
  a workflow-neutral interface that Phase 3 can consume; two-workflow reuse is
  verified and finalized for attribution in Phase 4.
- [ ] Interim shared growth is recorded before remove-specific workflow and
  integration scopes are added.

---

## Phase 3: Implement the Kernel Remove-Replica Workflow

### Objective

Implement the replayable remove-replica orchestration and prepared effects
without changing production routing.

### Changes Required

- **`kuberic-operator/Cargo.toml`**
  - Add the default-off `durable-remove-replica-pilot` feature so this phase's
    module and tests compile independently.
  - Include all dependencies required by a remove-only build:
    `dep:kuberic-durable-execution`,
    `kuberic-durable-execution/kubernetes`, and `dep:rand`.
  - Gate the shared modules when either durable feature is enabled and gate only
    the remove workflow module under the new feature. Production routing and
    runtime activation remain absent until Phase 4.
- **New `kuberic-operator/src/durable/remove_replica_pilot.rs`**
  - Define bounded, versioned workflow input from the accepted explicit remove
    operation, including mode, previous/reduced snapshots, exact target
    identity, admitted generation when required, and deadlines.
  - Define remove-specific workflow state, typed activity input/result,
    prepared activity kinds, and compact terminal outcome.
  - Drive `decide_remove_replica` as the semantic oracle.
  - Fold deterministic operation-state persistence, including commit-evidence
    recording, without creating redundant external activities.
  - Request authoritative passive observations for primary, correlated action,
    connection, target lifecycle, Pod UID, and role evidence.
  - Prepare exact replica commands, exact-UID role commands, and exact-UID Pod
    deletion commands through shared adapters.
  - Represent reduced stable-snapshot publication and final operation status in
    the terminal payload for post-terminal publication by the reconciler.
  - Define explicit activity, deterministic-transition, result, terminal, and
    encoded-checkpoint admission bounds. Reuse the stable encoded checkpoint
    and terminal payload ceilings unless evidence forces a separately reviewed
    change.
  - Add non-overlapping markers for the complete remove pilot module, comparable
    workflow scope, and workflow body.
- **`kuberic-operator/src/durable/remove_replica.rs`**
  - Make only the minimal visibility/interface changes needed for the kernel
    workflow to invoke the existing operation constructor/decision contract.
  - Preserve explicit callers and all existing decision tests.
- **`kuberic-operator/src/durable/effects.rs`**
  - Add remove-replica command allow-list validation and recovery/postcondition
    hooks using the Phase 2 shared interfaces.
  - Add a portable exact-UID Pod deletion command/outcome following the
    exact-UID label command pattern, but place its dedicated source boundary in
    `remove_integration` because remove-replica is its only consumer in this
    work item. Do not count it in `shared_after`.
- **Focused tests in `remove_replica_pilot.rs`**
  - Admission bounds and malformed input.
  - Deterministic success and safe compensation transcripts.
  - Typed unsafe ambiguity/quarantine outcomes.
  - Exact prepared replica/label/delete command identity.
  - ScaleDown authority and no ScaleDown-to-Force conversion.
  - Force-mode behavior without target authority.
  - Lost reply, proven-no-admission redelivery, busy retry, conflict, stale
    primary/target identity, and bounded generation redrive.
  - Restart/replay at every durable boundary.
  - Terminal reload without workflow polling and terminal accounting.
  - Maximum projected history, result reservation, active checkpoint, and
    terminal payload admission.

### Safety-Parity Test Traceability

The following test names are required so each Spec inventory item has a
reviewable pass/fail boundary:

| Spec safety item | Required test or trace |
|---|---|
| Admission/quorum | `remove_replica_pilot_rejects_primary_nonmember_uid_incarnation_minimum_and_quorum_drift` in `remove_replica_pilot.rs` |
| Primary authority | `remove_replica_pilot_rejects_generation_control_version_runtime_epoch_protocol_and_configuration_drift` in `remove_replica_pilot.rs` |
| ScaleDown/Force modes | `remove_replica_pilot_preserves_scale_down_authority_and_never_converts_to_force` and `remove_replica_pilot_force_retains_topology_and_primary_fences` |
| Coarse command identity | `remove_replica_pilot_replay_preserves_exact_coarse_action_identity` |
| Authoritative observations | `remove_replica_pilot_requires_correlated_primary_connection_lifecycle_uid_and_role_evidence` |
| Pre-commit retry classes | `remove_replica_pilot_distinguishes_refresh_busy_conflict_unknown_and_proven_no_admission` |
| Bounded redrive | `remove_replica_pilot_redrives_only_from_safe_configuration_within_attempt_bound` |
| Irreversible boundary | `remove_replica_pilot_persists_commit_before_cleanup_and_never_rolls_back_after_dispatch_ambiguity` |
| Cleanup | `remove_replica_pilot_requires_connection_retirement_uid_label_and_uid_delete_before_publication` |
| Terminal dispositions | `remove_replica_pilot_maps_success_compensation_and_unsafe_ambiguity` |
| Publication ordering | Phase 4 `test_durable_execution_remove_replica_pilot_terminal_precedes_status_publication` |
| Replay at every boundary | `remove_replica_pilot_restarts_at_every_durable_boundary_without_command_drift` |

Additional kernel-host obligations:

| Validation obligation | Required test or command |
|---|---|
| Typed input/result bounds | `remove_replica_pilot_rejects_oversized_typed_input_and_result` |
| Deterministic mismatch | `remove_replica_pilot_rejects_semantic_replay_mismatch` |
| Exposure before dispatch | `remove_replica_pilot_exposes_exact_command_before_dispatch` |
| Single-use remove permit | `remove_replica_pilot_consumes_delete_and_replica_permits_once` |
| Preparation rejection on identity drift | `remove_replica_pilot_rejects_preparation_after_primary_target_or_uid_drift` |
| Quarantined exposed reload | `remove_replica_pilot_reload_of_exposed_checkpoint_is_quarantined` |
| Checkpoint ownership/budget | Existing `cargo test -p kuberic-durable-execution --features kubernetes --test kubernetes_checkpoint` plus remove admission tests |
| Restart before dispatch | `remove_replica_pilot_restart_before_dispatch_preserves_command` |
| Restart after exposure | `remove_replica_pilot_restart_after_exposure_is_quarantined_without_second_permit` |
| Restart after observation | `remove_replica_pilot_restart_after_observation_replays_result` |
| Restart after commit | `remove_replica_pilot_restart_after_commit_rolls_forward` |
| Restart during cleanup | `remove_replica_pilot_restart_during_cleanup_preserves_uid_fences` |
| Restart after terminal | `remove_replica_pilot_terminal_reload_skips_workflow_polling` |
| Pre-commit compensation | `remove_replica_pilot_completes_only_proven_safe_compensation` |
| Terminal compaction | `remove_replica_pilot_terminal_checkpoint_discards_active_history_and_retains_accounting` |
| Shared adapter outcomes | `cargo test -p kuberic-operator --features durable-switchover-pilot,durable-remove-replica-pilot durable_effect_outcome` covering applied, definite failure, proven-no-admission, pending, and unknown |
| Replica-agent protocol | `cargo test -p kuberic-core replica_agent` and `cargo test -p kuberic-core replica_lifecycle` |

### Success Criteria

#### Automated Verification

- [ ] `cargo test -p kuberic-operator --features durable-remove-replica-pilot remove_replica_pilot`
- [ ] `cargo test -p kuberic-operator --features durable-switchover-pilot,durable-remove-replica-pilot`
- [ ] `cargo check -p kuberic-operator --all-targets --features durable-remove-replica-pilot`
- [ ] `cargo build -p kuberic-operator --all-targets --features durable-remove-replica-pilot`
- [ ] Existing `remove_replica` unit tests pass unchanged.
- [ ] `cargo test -p kuberic-durable-execution`
- [ ] `cargo test -p kuberic-durable-execution --features kubernetes`
- [ ] `cargo check --all-targets`
- [ ] `cargo build --all-targets`
- [ ] `cargo clippy --all-targets --all-features -- -D warnings`
- [ ] `cargo fmt --all -- --check`

#### Manual Verification

- [ ] The workflow-to-decision mapping covers every item in the Spec Safety
  Parity Inventory.
- [ ] Phase review checks the traceability tables above and records each named
  test as passing or identifies its Phase 4 reconciler dependency.
- [ ] No production reconciler path can select the new workflow in this phase.
- [ ] Workflow-specific lines are marked as `remove_body` or remove module
  scope rather than shared infrastructure.

---

## Phase 4: Integrate Operator Routing and Produce Measurements

### Objective

Add explicit opt-in routing, ConfigMap checkpoint reconciliation, terminal
publication, full fault coverage, and final measurable amortization evidence.

### Changes Required

- **`kuberic-operator/src/crd.rs`**
  - Add remove-replica execution mode with `Explicit` default and opt-in
    `DurablePilot` value.
  - Add a remove-specific durable execution reference containing version,
    execution ID, deterministic checkpoint name, and exact initial input.
  - Update schema/default tests and generated/deployed CRD assertions.
- **`kuberic-operator/deploy/deployment.yaml`**
  - Update the checked-in deployed CRD schema for the new mode and durable
    execution reference, matching generated schema tests.
- **`kuberic-operator/src/main.rs`**
  - Construct the shared durable runtime when either pilot feature is enabled.
- **`kuberic-operator/src/reconciler.rs`**
  - Validate unsupported durable remove selection without fallback.
  - Route all existing ScaleDown/Force selection sites through one
    mode-aware acceptance function.
  - Persist immutable execution authority before checkpoint creation and clear
    mutually exclusive explicit/durable state.
  - Add remove-specific kernel reconciliation using shared checkpoint,
    dispatch, observation, recovery, and measurement facilities.
  - Validate compact terminal output before publishing commit evidence,
    cleanup result, reduced stable topology, and completed/failed/poisoned
    status in terminal-before-status order.
  - Keep explicit routing unchanged when the new mode is omitted.
  - Add non-overlapping complexity markers for remove-specific operator
    integration and keep shared bridge ranges charged once.
- **`examples/kvstore/tests/reconciler.rs`**
  - Add feature-enabled durable remove happy path for a three-member ScaleDown
    to two members.
  - Verify a later/default removal uses the explicit path and does not reuse a
    retained durable reference.
  - Add every-turn restart, lost reply, proven-no-admission, checkpoint conflict,
    unknown checkpoint outcome, failed final status publication followed by
    terminal reload, stale UID/incarnation/generation, Force-mode authority,
    post-commit ambiguity, and repeated-execution identity cases.
  - Preserve the existing explicit remove, add, failover, lifecycle, and
    switchover matrices.
  - Use the concrete durable-remove prefix
    `test_durable_execution_remove_replica_pilot_` and include at least:
    `happy_path`, `default_remains_explicit`, `unsupported_feature_rejected`,
    `every_turn_restart`, `lost_reply`, `proven_no_admission`,
    `checkpoint_conflict`, `unknown_checkpoint_outcome`,
    `terminal_reload_after_status_failure`, `stale_uid`,
    `stale_incarnation`, `stale_generation`, `force_authority`,
    `post_commit_ambiguity`, and `repeated_execution_identity`.
- **`kuberic-operator/src/durable/pilot_store.rs` and measurement tests**
  - Add three identical no-fault three-member ScaleDown samples.
  - Print per-run and aggregate external-effect, passive-observation, durable
    boundary, accepted-write, active-byte, and terminal-byte evidence.
  - Assert stable admission ceilings while treating observed byte sizes as
    run-specific ranges.
- **`scripts/measure-switchover-complexity.py`**
  - Add `remove_body`, comparable remove workflow scope, `remove_integration`,
    and complete remove module diagnostics.
  - Measure the same shared label set as `shared_after`.
  - Calculate `shared_growth`, `remove_marginal`, both ratios, and
    positive/negative/inconclusive classification exactly as Specified.
  - Print component equations so the result is auditable.
  - Include both workflows in a single non-overlapping charged-scope validation;
    nested body/comparable diagnostics remain excluded from additive totals.
  - Retain the frozen `shared_before` values and revision from Phase 1.
- **`.github/workflows/CI.yml`**
  - Add focused durable remove measurement/admission tests using existing CI
    conventions without adding a new framework.

### Success Criteria

#### Automated Verification

- [ ] `cargo test -p kuberic-operator --features durable-switchover-pilot,durable-remove-replica-pilot`
- [ ] `CARGO_BUILD_JOBS=2 CARGO_INCREMENTAL=0 RUST_MIN_STACK=4194304 cargo test -p kvstore --test reconciler test_durable_execution_switchover_pilot_`
- [ ] `CARGO_BUILD_JOBS=2 CARGO_INCREMENTAL=0 RUST_MIN_STACK=4194304 cargo test -p kvstore --features durable-remove-replica-pilot --test reconciler test_durable_execution_remove_replica_pilot_`
- [ ] Existing explicit remove test ranges in
  `examples/kvstore/tests/reconciler.rs` pass.
- [ ] `cargo test -p kvstore --test reconciler test_durable_remove_`
- [ ] `cargo test -p kvstore --test reconciler test_durable_add_`
- [ ] `cargo test -p kvstore --test reconciler test_durable_failover_`
- [ ] `cargo test -p kuberic-core replica_agent`
- [ ] `cargo test -p kuberic-core replica_lifecycle`
- [ ] `cargo test -p kuberic-durable-execution`
- [ ] `cargo test -p kuberic-durable-execution --features kubernetes`
- [ ] `python3 scripts/measure-switchover-complexity.py`
- [ ] Complexity output has no overlap and prints every required formula,
  ratio, threshold, and classification.
- [ ] Three representative runs print stable counts and run-specific
  active/terminal byte ranges below the 770,048-byte encoded-checkpoint and
  4,096-byte terminal-payload ceilings.
- [ ] `cargo check --all-targets`
- [ ] `cargo build --all-targets`
- [ ] `cargo clippy --all-targets --all-features -- -D warnings`
- [ ] `cargo fmt --all -- --check`

#### Manual Verification

- [ ] Default explicit behavior is unchanged with and without the new feature.
- [ ] Unsupported opt-in fails instead of falling back.
- [ ] Terminal checkpoint acceptance precedes final status/topology publication.
- [ ] Every line added after Phase 1 is attributable to shared growth,
  remove-body, remove-integration, existing switchover integration, tests, or
  documentation without overlap.
- [ ] The amortization classification is accepted as measured even if negative
  or inconclusive.

---

## Phase 5: Correct Feasibility Evidence and Documentation

### Objective

Correct the runtime-neutrality evidence and write documentation against the
actual completed measurements rather than forecasts.

### Changes Required

- **`durable-execution/tests/feasibility.rs`**
  - Isolate the root library `[dependencies]` table using a table-aware helper
    rather than searching the complete manifest.
  - Keep the predicate tied to both the async store/host API evidence and the
    absence of a known async runtime in library dependencies.
  - Add focused manifest fixtures proving that a runtime in
    `[dev-dependencies]` passes while the same runtime in `[dependencies]`
    fails; include adjacent tables and feature references so the test is not a
    substring rubber stamp.
  - Re-run the complete classifier and capture the actual predicate statuses and
    final classification.
- **`durable-execution/README.md`**
  - Replace the stale write-efficiency-exception rationale with the actual
    recomputed classifier result and remaining failed predicate, if any.
  - Describe both explicitly gated kernel-hosted workflows and confirm that
    explicit execution remains the default.
  - Preserve stable admission contracts and label checkpoint byte values as
    run-specific measurements.
- **`docs/features/kuberic/durable-execution-roadmap.md`**
  - Update Current Boundary and Kubernetes-integration sections for the second
    authorized workflow.
  - Replace the untested amortization hypothesis with the exact Phase 4
    `shared_before`, `shared_after`, growth, marginal, ratio, and classification
    output.
  - Report remove-replica effects, passive observations, durable boundaries,
    accepted writes, and active/terminal byte ranges with the same precision
    distinctions used for switchover.
  - State clearly whether the result is positive, negative, or inconclusive and
    retain the default-off/no-broad-migration boundary.
- **`docs/features/kuberic/testing.md`**
  - Add feature-enabled durable remove unit, measurement, and reconciler
    commands following existing durable switchover documentation.
- **`.paw/work/durable-remove-replica/Docs.md`**
  - Load `paw-docs-guidance` and record the as-built architecture, selection
    contract, safety mapping, measurements, validation results, and known
    environment exception.

### Success Criteria

#### Automated Verification

- [ ] `CARGO_BUILD_JOBS=2 cargo test -p kuberic-durable-execution --test feasibility -- --nocapture`
- [ ] Feasibility output matches README wording exactly.
- [ ] `python3 scripts/measure-switchover-complexity.py`
- [ ] Roadmap numbers match script and representative measurement output.
- [ ] `cargo fmt --all -- --check`
- [ ] `cargo check --all-targets`
- [ ] `cargo build --all-targets`
- [ ] `cargo clippy --all-targets --all-features -- -D warnings`
- [ ] `cargo test -p kuberic-durable-execution`
- [ ] `cargo test -p kuberic-durable-execution --features kubernetes`
- [ ] Operator pilot and durable remove matrices pass.
- [ ] Add/remove/failover lifecycle regression suites pass.

#### Manual Verification

- [ ] README no longer attributes conditional feasibility to a removed
  write-efficiency exception.
- [ ] The runtime-neutrality predicate still fails for a real library runtime.
- [ ] Documentation distinguishes attempts from accepted writes, effects from
  observations, run-specific ranges from stable ceilings, and measured
  conclusions from hypotheses.
- [ ] No documentation claim is written before its evidence-producing command
  has run.
- [ ] If the live-cluster
  `kuberic-tests::kvstore_k8s::test_kvstore_k8s_write_read` test alone fails due
  to stale deployment, the exact failure is reported as an environment-related
  exception without modifying the cluster.

---

## Final Validation

Run from the proven execution checkout after all phases:

1. `cargo fmt --all -- --check`
2. `cargo check --all-targets`
3. `cargo build --all-targets`
4. `cargo clippy --all-targets --all-features -- -D warnings`
5. `cargo test -p kuberic-durable-execution`
6. `cargo test -p kuberic-durable-execution --features kubernetes`
7. `CARGO_BUILD_JOBS=2 cargo test -p kuberic-durable-execution --test feasibility -- --nocapture`
8. Existing switchover pilot unit and reconciler matrices.
9. New durable remove unit, measurement, and reconciler matrices.
10. Existing add-replica, explicit remove-replica, failover, and replica
   lifecycle suites.
11. `python3 scripts/measure-switchover-complexity.py`
12. `python3 -m unittest scripts.tests.test_measure_switchover_complexity`
13. `cargo test --all --all-features`, reporting only the specifically
    identified stale-cluster live test as an environment exception if observed.

## References

- Issue: none
- Spec: `.paw/work/durable-remove-replica/Spec.md`
- Research: `.paw/work/durable-remove-replica/SpecResearch.md`
- Code research: `.paw/work/durable-remove-replica/CodeResearch.md`
- Workflow context: `.paw/work/durable-remove-replica/WorkflowContext.md`
