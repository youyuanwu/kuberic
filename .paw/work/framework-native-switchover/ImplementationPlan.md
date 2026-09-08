# Framework-Native Switchover Graduation Implementation Plan

## Overview

Graduate the existing durable switchover pilot into the only production
switchover path. The implementation will keep the current pure switchover
decision model and independently correlated ReplicaAgent and exact-UID label
effects, but will place all admission, restart recovery, quarantine, terminal
reload, and publication behind the shared bounded durable runner and
ConfigMap checkpoint provider already used by framework-native
remove-replica.

The migration is intentionally replacement-oriented: first establish the
native contract, compatibility state, and safety coverage; then switch
admission and recovery to that path; finally remove the old selector, feature
gate, explicit production routing, duplicate status/condition surfaces, and
obsolete tests and documentation.

## Current State Analysis

- `kuberic-operator/src/durable/switchover.rs` is the shared pure decision
  state machine for both production-explicit and pilot execution. It owns the
  ordered success and compensation topology semantics
  (`kuberic-operator/src/durable/switchover.rs:106-419`,
  `kuberic-operator/src/durable/switchover.rs:449-598`).
- `kuberic-operator/src/durable/pilot.rs` already supplies the operation
  adapter for the shared runner, exact prepared effects, quarantine handling,
  terminal validation, projections, and measurements, but is version 2,
  feature-gated, limited to three replicas, and uses one 752 KiB ceiling for
  both active and terminal checkpoints
  (`kuberic-operator/src/durable/pilot.rs:68-76`,
  `kuberic-operator/src/durable/pilot.rs:1142-1496`).
- `kuberic-operator/src/reconciler.rs` has split admission, recovery,
  conditions, publication, and test wiring for `status.operation` versus
  `status.durableSwitchoverPilot`
  (`kuberic-operator/src/reconciler.rs:1521-1597`,
  `kuberic-operator/src/reconciler.rs:1788-1820`,
  `kuberic-operator/src/reconciler.rs:2959-3099`).
- `kuberic-operator/src/crd.rs` and the checked-in deployment schema expose
  the execution-mode selector and both persisted surfaces
  (`kuberic-operator/src/crd.rs:46-56`,
  `kuberic-operator/src/crd.rs:115-139`,
  `kuberic-operator/src/crd.rs:194-215`).
- Framework-native remove-replica establishes the current native reference,
  incompatibility-marker, shared-runner, independent-bound, exact one-over,
  publication, and legacy-conversion patterns
  (`kuberic-operator/src/reconciler.rs:290-399`,
  `kuberic-operator/src/durable/remove_replica_execution.rs:71-83`,
  `kuberic-operator/src/durable/remove_replica_execution.rs:4786-4863`).
- Current pilot coverage already proves the canonical 9 external effects,
  3 passive observations, 12 boundaries, and 13 accepted writes, plus
  restart, persistence ambiguity, exact command identity, compensation,
  terminal reload, and 19/21-record maximum projections
  (`kuberic-operator/src/durable/pilot.rs:4428-4537`,
  `examples/kvstore/tests/reconciler.rs:3630-5127`).
- The current protocol has no coarse switchover intent or peer stage. The
  workflow spans multiple replica processes and Kubernetes routing effects,
  so the plan retains individually correlated actions under the global
  checkpoint (`kuberic-core/src/types.rs:448-489`,
  `kuberic-core/proto/kuberic.proto:251-271`,
  `kuberic-operator/src/durable/effects.rs:102-147`).

## Architecture Decisions and Trade-offs

### Retain individually correlated actions

- **Selected**: Keep the current exact per-command identity and observation
  boundary while moving the global execution exclusively to the framework.
  This retains deterministic action IDs, incarnation/epoch/postcondition
  evidence, generation/control-version/runtime-epoch fences, and exact-UID
  routing effects already implemented
  (`kuberic-operator/src/crd.rs:794-819`,
  `kuberic-operator/src/durable/effects.rs:204-330`,
  `kuberic-operator/src/durable/effects.rs:405-470`).
- **Rejected**: Add one coarse primary-agent switchover intent. It would reduce
  operator-visible local steps, but the existing protocol has no switchover
  intent or peer stage, the sequence crosses the old primary, target, retained
  replicas, and Kubernetes routing objects, and local agent history remains
  bounded and process-local (`kuberic-core/src/replica_lifecycle.rs:125-180`,
  `kuberic-core/src/replica_agent.rs:38-41`,
  `kuberic-core/src/replica_agent.rs:1715-1743`). Implementing it would require
  a new distributed coordinator without improving the already-proven exact
  recovery boundary.

### Fail closed instead of migrating legacy histories

- **Selected**: Convert explicit v1 and pilot v1/v2 status into typed,
  fingerprinted incompatibility evidence before ordinary reconciliation,
  following native remove-replica's atomic conversion pattern
  (`kuberic-operator/src/reconciler.rs:290-399`).
- **Rejected**: Translate old activity history into the new contract. Pilot v1
  and v2 use different activity contracts, and the kernel validates immutable
  workflow input, activity identity, and envelope shape separately
  (`durable-execution/src/checkpoint.rs:8-145`,
  `kuberic-operator/src/durable/pilot.rs:329-363`,
  `kuberic-operator/src/durable/pilot.rs:795-841`). Translation would require
  inventing execution evidence that is not present.

### Independent measured bounds with explicit headroom

- **Selected**: Use maximum encoded fixtures and canonical success/rollback
  projections to set separate active, terminal, and payload limits, retaining
  enough explicit headroom for the 19- and 21-record histories. Validate every
  declared dimension at its exact bound and one unit over.
- **Rejected**: Copy remove-replica limits or retain the pilot's shared
  752 KiB active/terminal ceiling. Current switchover measurements are about
  31.6 KiB active and 4.1 KiB terminal, while remove-replica has a different
  contract and history shape
  (`docs/features/kuberic/testing.md:410-446`,
  `kuberic-operator/src/durable/remove_replica_execution.rs:71-83`).
  Final numeric values will be selected from encoded maximum fixtures in
  Phase 1 and must keep the projected maximum below the admitted bound while
  exact one-over cases fail.

## Desired End State

- Every newly admitted switchover creates one immutable native execution
  reference in status and one owner-bound ConfigMap checkpoint contract.
- Every active switchover, including terminal publication retry, runs through
  the shared bounded durable runner with no build feature or public selector.
- Explicit version-1 status and pilot version-1/version-2 status are converted
  to durable incompatibility evidence and never resumed or treated as absence.
- The native contract has a new independent version, compact execution
  reference, separate active and terminal encoded limits, and measured
  switchover-specific input/result/payload/history/fuel bounds.
- The existing pure decision model continues to define write revocation,
  frozen-LSN catch-up, demotion/promotion, epoch and configuration sequencing,
  compensation, final attestation, and exact topology publication.
- Only one switchover status/condition/resource vocabulary remains.
- The normal workspace build, manifests, examples, CI, tests, and
  documentation describe and validate the graduated path.

## What We're NOT Doing

- Adding a coarse `SwitchoverIntent`, new lifecycle-peer stage, or other
  agent-to-agent switchover coordinator.
- Migrating add-replica, failover, or initial partition creation to the
  framework.
- Changing framework-native remove-replica behavior.
- Addressing SQLite issue #42.
- Adding a worker, queue, lease, watcher, separate durable service, generic
  orchestration runtime, or automatic unknown-effect retry.
- Resuming or translating legacy explicit/pilot checkpoint history into the
  new contract.
- Redesigning the general ReplicaAgent, lifecycle-peer, or gRPC APIs.

## Phase Status

- [x] **Phase 1: Native Contract and Compatibility Surface** - Introduce the
  sole versioned switchover execution reference, incompatibility evidence, and
  independent contract bounds while preserving existing routing temporarily.
- [x] **Phase 2: Native Admission, Recovery, and Publication Path** - Add the
  complete native path and fail-closed legacy conversion alongside the old
  production routing so replacement behavior can be proven before cutover.
- [x] **Phase 3: Replacement Safety Coverage and Cutover** - Prove the native
  path covers the complete safety matrix, then make it the sole admission and
  recovery route.
- [x] **Phase 4: Remove Obsolete Surfaces and Update Delivery Artifacts** -
  Delete the selector, build feature, split routing/status/conditions, stale
  test helpers, and update the checked-in schema, examples, and CI gates.
- [x] **Phase 5: Documentation and Full Validation** - Record the as-built
  architecture and measurements, update project documentation, and run the
  complete repository validation matrix.

## Phase Candidates

<!-- Intentionally empty. New work must remain within the specification. -->

## Phase Dependencies and Promotion Gates

- Phase 2 starts only after Phase 1 contract/reference serialization,
  maximum-fixture admission, and compatibility marker tests pass.
- Phase 2 adds testable native admission/recovery/publication components but
  does not activate them in production routing and does not convert live
  legacy status.
- Phase 3 starts only after the native components can complete and compensate
  through direct test entry points without changing default production
  selection. Phase 3 is the single activation point: all native unit, bridge,
  reconciler, legacy, recovery, measurement, and adjacent-operation targeted
  tests must pass before one change activates native admission, legacy
  conversion, native recovery, and old-route retirement together.
- Phase 4 starts only after cutover tests pass; it removes old public/build
  surfaces but not compatibility readers.
- Phase 5 starts only after repository searches and schema parity prove the
  obsolete surfaces are gone.

## Requirements Traceability

| Spec item | Owning phase(s) | Primary verification |
|---|---|---|
| FR-001, SC-001 | 2, 3, 4 | Selector-free native happy path and schema search |
| FR-002 | 1, 2, 3 | Already-primary, non-member, version/target/incarnation admission tests |
| FR-003, FR-004 | 2, 3 | Ordered 9-effect success transcript and catch-up timeout tests |
| FR-005, FR-006, SC-002 | 1, 2, 3 | Exact command, lost reply, quarantine, stale UID/generation tests |
| FR-007 | 2, 4 | Reconciler-only execution architecture and absence searches |
| FR-008, FR-009 | 2, 3 | Conflict/unknown reload, terminal reload, publication retry tests |
| FR-010, SC-003 | 2, 3 | Pre-promotion and uncertain-promotion compensation matrices |
| FR-011, SC-005 | 1, 3, 5 | Command identity tests and protocol/documentation absence checks |
| FR-012, FR-013 | 1, 3 | Independent contract declarations and exact one-over tests |
| FR-014, SC-006, SC-007 | 1, 3 | 9/3/12/13 measurement and 19/21 projections |
| FR-015, FR-016, SC-004 | 1, 2, 3 | Explicit v1, pilot v1/v2, malformed status/reference, unsupported envelope/contract/activity/terminal fixtures |
| FR-017 | 3, 4 | Cutover only after replacement matrix passes; old-surface absence search |
| FR-018, SC-008 | 4, 5 | Schema, examples, CI, `just images`, and full repository gates |
| FR-019, SC-009 | 5 | Docs.md and project documentation review/search |
| FR-020, SC-010 | 1-5 | Named adjacent-operation regression suites and full workspace gate |

---

## Phase 1: Native Contract and Compatibility Surface

### Changes Required

- **`kuberic-operator/src/crd.rs`**:
  - Add one production switchover execution status type modeled on the native
    remove-replica reference: immutable admitted identity and checkpoint
    reference or typed incompatibility evidence.
  - Give incompatibility evidence explicit source categories for legacy
    explicit operation v1, pilot v1, pilot v2, and malformed/unsupported
    shapes, retaining source version, identity, checkpoint name when present,
    and a stable fingerprint.
  - Preserve unknown legacy status fields long enough for one reconcile to
    classify them before replacement status is serialized.
- **`kuberic-operator/src/durable/pilot.rs` and
  `kuberic-operator/src/durable/mod.rs`**:
  - Move the pilot implementation to
    `kuberic-operator/src/durable/switchover_execution.rs` and promote it into
    an unconditionally compiled native switchover execution module and
    vocabulary.
  - Define a new immutable contract/reference version and native workflow key,
    activity identity, terminal identity, and checkpoint-name validation.
  - Keep the pure decision model in `durable/switchover.rs`; do not introduce
    a coarse agent intent.
  - Separate active checkpoint, terminal checkpoint, terminal payload,
    decoded workflow input, decoded activity input, decoded result, activity
    count, replica count, error text, workflow transition, and runner-fuel
    limits.
  - Derive initial conservative values from maximum switchover fixtures and
    expose measurement helpers so later phases can tighten and document them.
- **`kuberic-operator/src/durable/checkpoint_store.rs`**:
  - Make native switchover activity/terminal measurement decoding available
    in ordinary builds alongside native remove-replica decoding, without a
    feature-dependent default.
- **Tests in the same modules**:
  - Validate the native reference and deterministic checkpoint identity.
  - Validate incompatibility marker serialization, fingerprints, and
    immutable admission fields.
  - Add maximum encoded fixtures used to select each new contract dimension,
    following the remove-replica execution pattern. Phase 3 owns the complete
    exact-bound and one-unit-over rejection matrix.
  - Retain exact replay-byte, transition-fuel, accounting-reachability, owner,
    permit, command-fence, and terminal-validation tests under native names.

### Success Criteria

#### Automated Verification

- [ ] `cargo fmt --all -- --check`
- [ ] `cargo test -p kuberic-operator framework_native_switchover`
- [ ] `cargo test -p kuberic-operator switchover_execution`
- [ ] Maximum encoded fixtures establish candidate values for every
  independent contract dimension.
- [ ] Existing remove-replica execution tests pass unchanged.

#### Manual Verification

- [ ] The new reference cannot represent both admitted and incompatible state.
- [ ] Legacy source identity is retained without authorizing execution.
- [ ] No coarse switchover intent or peer protocol is present in the diff.

---

## Phase 2: Native Admission, Recovery, and Publication Path

### Changes Required

- **`kuberic-operator/src/reconciler.rs`**:
  - Construct and share the durable workflow runtime unconditionally for
    switchover and remove-replica.
  - Add an internal native admission component that explicitly rejects already-primary and
    non-member targets, binds the contract version, target, stable snapshot,
    and exact member incarnations, persists the immutable execution reference
    before first checkpoint/effect, and records one native condition.
  - Add, but do not yet invoke from production routing, the
    pre-reconciliation conversion component for recognized explicit v1 and
    pilot v1/v2 state. Conversion behavior must be atomic, stable across
    retries, and ordered before ordinary switchover progress when activated in
    Phase 3.
  - Add a direct native recovery component that routes every valid native
    active switchover reference through the shared runner;
    missing, malformed, incompatible, rejected, isolated, nondeterministic,
    persistence, reload, and deadline outcomes must map to the sole status and
    condition vocabulary.
  - Preserve terminal-before-publication ordering, terminal reload without
    observation polling, exact member/UID/epoch/role/configuration/routing
    validation, election metadata refresh, committed status persistence,
    measurement logging, and process-local host eviction.
  - Keep `durable/switchover.rs` as the operation-specific deterministic
    reducer consumed by the adapter.
- **`kuberic-operator/src/durable/effects.rs`**:
  - Compile switchover command preparation, dispatch, result classification,
    and quarantine handling unconditionally.
  - Preserve one-use permit consumption, exact encoded command identity,
    process generation/control-version/runtime-epoch fences, bounded proven
    non-admission redelivery, and exact-UID label observation.
- **`kuberic-operator/src/main.rs` and constructor call sites**:
  - Use the ordinary runtime-enabled reconciler construction without a pilot
    feature branch.
- **Reconciler and integration tests**:
  - Cover admission ordering, no-effect status conflicts, all runner outcomes,
    fail-closed legacy conversion, missing-current-checkpoint ordering,
    success publication, compensation publication, terminal reload, and
    publication retry through direct native test entry points while the
    production selector and old recovery routing remain unchanged.

### Success Criteria

#### Automated Verification

- [ ] `cargo check -p kuberic-operator --all-targets`
- [ ] `cargo test -p kuberic-operator framework_native_switchover`
- [ ] Direct operator routing tests cover native admission, compatibility
  conversion, runner dispositions, terminal reload/publication, and
  publication retry cleanup. The full kvstore matrix remains the Phase 3
  cutover gate.

#### Manual Verification

- [ ] Direct native admission produces one native reference and condition.
- [ ] Direct conversion of an explicit v1 or pilot v1/v2 fixture produces a
  stable incompatibility marker and never dispatches.
- [ ] A terminal checkpoint can be republished after process restart without
  collecting replica observations first.
- [ ] Adjacent add, remove, failover, and initial-create routing is unchanged.

---

## Phase 3: Replacement Safety Coverage and Cutover

### Changes Required

- **`kuberic-operator/src/durable/switchover_execution.rs`**:
  - Rename pilot tests and fixtures to native terminology.
  - Preserve coverage for exact replay bytes, semantic drift,
    activity-version drift, one-use permits, owner validation, quarantine,
    conflict/unknown persistence reload, nondeterminism, compensation terminal
    validation, and exact reachable accounting.
  - Add exact one-unit-over coverage for activity count, workflow input,
    activity input, result, active checkpoint, terminal checkpoint, terminal
    payload, error text, replica count, transition fuel, and runner fuel.
  - Record the maximum projected 19-record success-with-redelivery and
    21-record rollback-with-redelivery sizes against the independent limits.
- **`kuberic-operator/src/reconciler.rs` tests**:
  - Retain all seven ReplicaAgent command bridge cases, exact snapshot-UID
    label commands, stale-generation preparation, bounded redelivery, and
    label quarantine under native compilation.
- **`examples/kvstore/tests/reconciler.rs`**:
  - Convert the pilot matrix into the canonical selector-free native matrix.
  - Preserve restart-after-every-turn, lost-reply windows, failed-promotion
    compensation, stale incarnation, unknown checkpoint outcomes, operation
    outcome matrix, terminal CAS conflict, and publication reload.
  - Add fixtures for explicit v1, pilot v1, pilot v2, malformed reference,
    malformed status, unsupported contract, unsupported activity version,
    unsupported terminal shape, unsupported envelope, and lost/inconsistent
    checkpoint identity.
  - Report terminal-payload bytes in addition to active bytes, terminal bytes,
    effects, observations, boundaries, and accepted writes.
- **`durable-execution` provider tests where integration is affected**:
  - Re-run deterministic Kubernetes checkpoint ownership, CAS, unknown
    outcome, and active/terminal validation coverage.
- **`kuberic-operator/src/reconciler.rs` cutover**:
  - After the complete native unit and integration replacement matrix passes,
    activate legacy conversion, native admission, and native recovery in one
    production routing change; remove explicit and pilot admission/recovery
    selection so every new and valid active switchover uses the native path.
  - Leave legacy fields readable only for incompatibility conversion; do not
    delete their schema surfaces until Phase 4.

### Success Criteria

#### Automated Verification

- [ ] Canonical native happy path reports exactly 9 external effects,
  3 passive observations, 12 completed boundaries, and 13 accepted writes.
- [ ] Measurement output separately reports maximum active checkpoint,
  terminal checkpoint, and terminal-payload bytes.
- [ ] 19-record success and 21-record rollback projections fit their admitted
  limits.
- [ ] Every independent one-unit-over test fails before unsafe dispatch or an
  unpersistable terminal.
- [ ] `CARGO_BUILD_JOBS=2 CARGO_INCREMENTAL=0 RUST_MIN_STACK=4194304 cargo test -p kvstore --test reconciler test_framework_native_switchover_ -- --nocapture`
- [ ] `cargo test -p kuberic-durable-execution --features kubernetes --test kubernetes_checkpoint`

#### Manual Verification

- [ ] Test names and assertions describe one native path rather than a pilot.
- [ ] Removed explicit tests have a mapped native replacement for every safety
  scenario.
- [ ] Bounds are traceable to switchover fixtures and measurements, not
  remove-replica constants.
- [ ] Cutover occurs only after the complete replacement matrix has passed.
- [ ] After cutover, every new request uses one native reference and condition,
  and every recognized old in-progress state converts rather than resumes.

---

## Phase 4: Remove Obsolete Surfaces and Update Delivery Artifacts

### Changes Required

- **`kuberic-operator/src/crd.rs`**:
  - Remove `spec.switchoverExecutionMode`, `SwitchoverExecutionMode`, and the
    public `status.durableSwitchoverPilot` schema surface after native status
    and compatibility conversion are active.
  - Retain `status.operation` because create, add, and failover still use it;
    remove only switchover admission/recovery/publication writes to that field.
    Continue recognizing `kind=Switchover` version-1 values as legacy
    incompatibility input.
  - Retain a deserialization-only legacy pilot reader, excluded from the
    generated public schema and omitted from serialized replacement status, so
    pilot v1/v2 references remain recognizable after the public field is
    removed.
  - Update serialization, schema, printer, unknown-field, and checked-in
    manifest parity tests for the sole execution surface.
- **`kuberic-operator/Cargo.toml`,
  `examples/kvstore/Cargo.toml`,
  `kuberic-operator/src/durable/mod.rs`,
  `kuberic-operator/src/durable/effects.rs`,
  `kuberic-operator/src/durable/checkpoint_store.rs`, and
  `kuberic-operator/src/reconciler.rs`**:
  - Remove the `durable-switchover-pilot` feature and development dependency
    opt-in.
  - Remove feature-gated constructor, import, runtime, decoder, effect, and
    test branches.
- **`kuberic-operator/deploy/deployment.yaml` and
  `examples/kvstore/deploy/kubericset.yaml`**:
  - Regenerate/update the checked-in CRD schema and examples so no selector or
    duplicate pilot field remains and the native execution status schema is
    present.
  - Preserve ConfigMap writer RBAC required by both native workflows.
- **`.github/workflows/CI.yml`**:
  - Replace pilot-feature commands with ordinary native switchover projection,
    bound, measurement, and integration commands.
  - Preserve format, clippy, build, images, all-features, and provider gates.
- **`kuberic-operator/src/reconciler.rs`,
  `kuberic-operator/src/crd.rs`,
  `kuberic-operator/src/durable/mod.rs`,
  `kuberic-operator/src/durable/switchover_execution.rs`, and
  `examples/kvstore/tests/reconciler.rs`**:
  - Remove explicit-only production routing helpers and superseded selector,
    feature, condition, resource, and pilot-named fixtures after verifying
    replacement coverage.
  - Do not remove the deterministic switchover decision reducer or reusable
    safety-validation tests it still serves.

### Success Criteria

#### Automated Verification

- [ ] `cargo check --all-targets`
- [ ] `cargo fmt --all -- --check`
- [ ] `cargo clippy --all-targets -- -D warnings`
- [ ] `cargo build --all-targets`
- [ ] `cargo test -p kuberic-operator`
- [ ] CRD parity tests pass against `deployment.yaml`.
- [ ] Repository search finds no production selector, pilot feature, pilot
  condition, or pilot status field.

#### Manual Verification

- [ ] Ordinary builds always include native switchover.
- [ ] ConfigMap writer RBAC remains least-privilege and unchanged in scope.
- [ ] No intermediate branch or PR is created; changes remain on the target
  branch.

---

## Phase 5: Documentation and Full Validation

### Changes Required

- **`.paw/work/framework-native-switchover/Docs.md`**:
  - Create the as-built technical reference using `paw-docs-guidance`,
    including the selected ownership boundary, native status and checkpoint
    lifecycle, compatibility behavior, independent bounds, measurements,
    recovery/quarantine semantics, publication ordering, and verification
    commands.
- **`kuberic-operator/README.md`**:
  - Replace explicit-default/pilot opt-in instructions with the sole native
    workflow, current status/condition shape, persistence behavior, and
    measurements.
- **`docs/features/kuberic/operator.md`**:
  - Document native admission, shared-runner responsibility, operation
    adapter responsibility, compatibility markers, terminal reload, and
    publication.
- **`docs/features/kuberic/protocols.md`**:
  - Preserve the ordered correlated-action protocol and record that
    switchover deliberately remains globally checkpoint-owned rather than
    introducing a coarse agent intent.
- **`docs/features/kuberic/testing.md`**:
  - Document selector-free commands, replacement test inventory, independent
    bounds, exact 9/3/12/13 baseline, 19/21 projections, and measured active,
    terminal, and payload bytes.
- **`docs/features/kuberic/durable-execution-roadmap.md` and
  `docs/features/kuberic/status.md`**:
  - Mark switchover graduated, remove the optional-pilot and open
    agent-owned-candidate language, and record the evidence-based retained
    ownership boundary.
- **`docs/features/kuberic/design-gaps.md` and
  `docs/features/kuberic/rolling-upgrade-design.md`**:
  - Update statements that materially describe the old execution split while
    preserving historical safety rationale.
- **Full validation**:
  - Run all authoritative non-cluster, documentation, build, lint, full
    workspace, measurement, and deterministic provider gates.
  - Run the real-API provider test only when an authorized cluster context is
    available; otherwise record that environmental limitation in `Docs.md`.

### Success Criteria

#### Automated Verification

- [ ] `cargo check --all-targets`
- [ ] `cargo fmt --all -- --check`
- [ ] `cargo clippy --all-targets -- -D warnings`
- [ ] `cargo build --all-targets`
- [ ] `cargo test -p kuberic-core -p kuberic-operator -p kvstore -p sqlite-replicated`
- [ ] `cargo test --doc --workspace`
- [ ] `cargo test -p kuberic-durable-execution --features kubernetes --test kubernetes_checkpoint`
- [ ] `cargo test --all --all-features`
- [ ] `just images`
- [ ] Targeted native switchover measurements complete with the documented
  values.

#### Manual Verification

- [ ] Documentation contains no opt-in selector or durable-pilot instructions.
- [ ] Documentation explicitly records why no coarse switchover intent was
  introduced.
- [ ] The final diff contains no changes to
  `.paw/work/framework-native-remove-replica/`.
- [ ] Every commit contains the required Copilot co-author trailer.

### Adjacent Operation Regression Inventory

- [ ] `cargo test -p kuberic-operator remove_replica_execution`
- [ ] `cargo test -p kvstore --test reconciler test_durable_add`
- [ ] `cargo test -p kvstore --test reconciler test_durable_failover`
- [ ] `cargo test -p kvstore --test reconciler test_durable_initial`
- [ ] `cargo test -p kuberic-core replica_agent`

---

## References

- Issue: none
- Spec: `.paw/work/framework-native-switchover/Spec.md`
- Research:
  `.paw/work/framework-native-switchover/SpecResearch.md`,
  `.paw/work/framework-native-switchover/CodeResearch.md`
- Read-only precedent:
  `.paw/work/framework-native-remove-replica/`
