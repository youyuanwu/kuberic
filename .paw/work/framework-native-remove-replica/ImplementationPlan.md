# Framework-Native Remove Replica Implementation Plan

## Overview

Graduate remove-replica from the explicit-versus-durable pilot into one default,
framework-native production workflow. The implementation will preserve the
existing remove domain decisions and replica protocol, move common durable host
recovery into a shared in-process runner used by switchover and remove-replica,
replace snapshot-heavy activity records with a compact replay contract, reject
legacy contracts explicitly, and remove every remove-specific mode, feature,
status, condition, telemetry, comparison, and documentation surface.

Planning and implementation use the `single-model` mode configured in
`WorkflowContext.md`, with `gpt-5.6-sol` as the only model. The review strategy
is `local`: each implementation phase is committed directly to
`feature/framework-native-remove-replica` with the required
`[Framework-Native Remove Replica]` title prefix and Co-authored-by trailer.
There are no planning, phase, or documentation branches and no intermediate
pull requests. This planning activity creates no commit.

## Current State Analysis

- New remove requests default to the explicit `status.operation` state machine.
  A default-off Cargo feature plus `spec.removeReplicaExecutionMode:
  durablePilot` selects a second path persisted through
  `status.durableRemoveReplicaPilot`.
- Switchover and durable remove already share the durable kernel, measured
  checkpoint store, workflow-keyed host cache, prepared activity validation,
  one-use dispatch permits, and effect vocabulary. Their complete
  load/reload/quarantine/permit/terminal/requeue loops remain duplicated in
  `kuberic-operator/src/reconciler.rs`.
- The remove pilot serializes a complete mutable operation projection in every
  activity input and successful result. It also duplicates the initial
  operation between status and workflow input and repeats terminal authority
  fields. The current representative maximum active record is 93,841 bytes.
- The kernel enforces one encoded checkpoint ceiling for both active and
  terminal records. The approved contract requires independent 262,144-byte
  active and 12,288-byte terminal encoded ceilings, in addition to the
  4,096-byte terminal payload ceiling.
- Most safety invariants already have native tests. Exact primary/target status
  gaps without churn, remove-specific incompatible/corrupt records, and
  retained-record cleanup authorization need named replacement coverage before
  explicit remove code can be deleted.
- Project documentation, generated CRD schema, CI, example features, conditions,
  measurements, and complexity reporting still describe a comparison pilot.

## Desired End State

- Every newly accepted remove request creates one
  `status.removeReplicaExecution` reference and executes through the durable
  kernel without a spec selector or build feature.
- A shared operator runner owns the bounded host lifecycle for switchover and
  remove-replica. Operation adapters retain the six independently testable
  responsibilities: observation collection, authority/preparation, exact
  effect dispatch and quarantine interpretation, deadline policy, terminal
  validation, and publication/conditions.
- The remove workflow replays from one compact immutable admission input and a
  history of exact commands and compact evidence/results. Boundary records do
  not carry complete mutable operation state or complete multi-configuration
  state snapshots.
- The representative no-fault three-member ScaleDown path remains exactly
  three external effects, two passive observations, five durable boundaries,
  and six accepted writes. Every one of at least three representative samples
  has a maximum active encoded record no larger than 49,152 bytes.
- Fault admission rejects the seventeenth history record, a 4,097-byte input, a
  2,049-byte result, a 262,145-byte active record, a 12,289-byte terminal
  record, and a 4,097-byte terminal payload.
- Unsupported native contract versions, legacy pilot references/checkpoints,
  and legacy explicit remove records produce a typed incompatible outcome.
  They are never resumed as the new contract, converted, cleared, or treated as
  permission to start a fresh execution.
- Explicit remove orchestration and all remove pilot product surfaces are gone
  only after the complete SC-002 safety inventory maps to passing native/shared
  replacement tests.
- Full repository validation passes against the dedicated Kuberic Kind cluster
  named `kind` using only `~/.kube/kuberic-kind-config`.

## Architecture Decisions

1. **Place the shared runner in the operator, not the kernel.**
   - Selected: add `kuberic-operator/src/durable/runner.rs` as an in-process
     orchestration component over existing `DurableHost` APIs.
   - Rejected: expanding `durable-execution` with Kubernetes/replica semantics,
     because the kernel is already workflow-neutral.
   - Rejected: macros or copied helper loops, because they would preserve hidden
     lifecycle duplication and weaken direct common-outcome testing.

2. **Use an adapter boundary for operation-specific safety.**
   - The runner owns load, terminal short-circuit, bounded fuel, permit
     consumption, fused observation/progression, quarantine, reload, storage
     failure, nondeterminism, rejection, and deadline-clamped requeue handling.
   - Switchover and remove adapters own the six responsibilities required by
     FR-019. Publication remains behind operation-specific terminal validation;
     the runner never interprets remove authority or topology.

3. **Use a compact event/evidence contract rather than repeated snapshots.**
   - The immutable remove input contains only contract/execution identity,
     accepted operation identity and mode, one previous stable snapshot, exact
     target identity/UID, minimum/quorum inputs, and deadlines. The reduced
     snapshot is derived deterministically instead of persisted beside the
     previous snapshot.
   - The status reference stores the same structured compact immutable input
     needed to reconstruct the exact `ExecutionSpec`; it does not store a
     JSON-wrapped `DurableOperationStatus`.
   - Activity inputs are a tagged passive-observation request or an exact
     prepared replica/label/delete command. Results are compact typed evidence,
     effect outcomes, or bounded proven-no-admission redelivery evidence.
     Workflow replay folds these records into transient state.
   - Terminal completion stores commit evidence, cleanup proof, and accounting
     once. Authority duplicated by the immutable input is validated by
     reference rather than copied into a second terminal structure.
   - Active-history compaction or Continue-as-New is not used to conceal an
     oversized authoring contract.

4. **Enforce active and terminal encoded limits independently.**
   - Extend `CheckpointLimits` with a terminal encoded ceiling while preserving
     the existing active pre-decode ceiling. Decode is still protected by the
     active ceiling; terminal payloads receive the stricter post-decode
     12,288-byte record check.
   - The remove contract declares 16 records, 4,096-byte inputs, 2,048-byte
     results, 262,144-byte active records, 12,288-byte terminal records, and
     4,096-byte terminal payloads. Admission projects the maximum fault history
     before the first checkpoint is created.
   - Encoded limits are immutable constants of a workflow contract version.
     Switchover keeps a terminal ceiling equal to its prior single encoded
     ceiling, preserving existing checkpoint reloads. Native remove selects its
     exact active/terminal limits from the serialized production contract
     version before host construction. Any future limit change requires a
     contract-version bump and explicit compatibility behavior.

5. **Fail closed across the clean break.**
   - Accept the legacy pilot field only during deserialization. On the first
     reconcile, atomically replace it with a serialized production
     `status.removeReplicaExecution` incompatibility marker containing the
     legacy source kind, contract version, execution identity, and stable
     fingerprint. The removed legacy field may disappear only in the same
     status write that durably creates this marker.
   - Apply the same durable-marker conversion to old explicit
     `DurableOperationKind::RemoveReplica` status. Repeated reconciles and
     restarts continue returning `IncompatibleContract` from the production
     marker and can never interpret marker presence as permission to admit a
     fresh execution.
   - Legacy pilot references, old workflow/activity versions, and old explicit
     remove records map to `IncompatibleContract`. Malformed, oversized,
     wrong-owner, or wrong-execution records retain distinct
     rejection/isolation outcomes.

6. **Retain the existing protocol and deployment topology.**
   - `ReplicaAgent`, gRPC contracts, exact remove intent signatures, Kubernetes
     effects, checkpoint owner references, and separate cleanup RBAC remain the
     authority boundary.
   - No worker, queue, lease, watcher, scheduler, or add-replica port is added.

## What We're NOT Doing

- Migrating add-replica to the durable kernel.
- Changing replica control protocol versions, gRPC messages, agent action
  ledger behavior, or remove intent signatures.
- Generalizing remove beyond the current maximum three-member domain.
- Adding a checkpoint migration/converter for pilot or explicit remove records.
- Adding a deployed executor, queue, lease, watcher, distributed owner, or
  retry scheduler.
- Changing switchover from its current public pilot selection model.
- Using active-history compaction or Continue-as-New to pass the 48 KiB gate.
- Optimizing solely for lexical source-complexity ratios.
- Creating intermediate branches or pull requests.

## Phase Status

- [ ] **Phase 1: Production Durable Infrastructure** - Add independent checkpoint ceilings, production-neutral measurement naming, and the shared bounded runner contract.
- [ ] **Phase 2: Switchover Runner Adoption** - Move switchover onto the shared runner without changing its operation-specific behavior.
- [ ] **Phase 3: Compact Native Remove Contract** - Build the versioned compact remove workflow, explicit incompatibility handling, and bounded admission.
- [ ] **Phase 4: Native Integration and Safety Traceability** - Integrate remove with the runner and establish passing replacement coverage for every deletion-gate invariant.
- [ ] **Phase 5: Production Graduation and Legacy Removal** - Make native remove the only production path and delete explicit/pilot surfaces after the safety gate passes.
- [ ] **Phase 6: Measurements and Full Validation** - Lock semantic/byte measurements, update CI, and run complete local plus isolated-Kind validation.
- [ ] **Phase 7: Documentation** - Produce the as-built record and update all project documentation for the production contract.

## Phase Candidates

---

## Phase 1: Production Durable Infrastructure

### Changes Required

- **`durable-execution/src/checkpoint.rs`**:
  - Extend `CheckpointLimits` to distinguish maximum active encoded bytes from
    maximum terminal encoded bytes.
  - Preserve the active envelope limit before JSON decoding, then enforce the
    lifecycle-specific active or terminal limit after decoding.
  - Add typed errors/accessors for a zero terminal limit and terminal encoded
    overflow without changing envelope format version 3.
- **`durable-execution/src/host.rs`**:
  - Route all proposed and loaded terminal checkpoints through the new terminal
    encoded limit while preserving conflict/outcome-unknown reload semantics.
- **`durable-execution/tests/replay.rs`**:
  - Add exact-limit and one-byte-over tests for active and terminal encoded
    records, and verify oversized terminal rejection occurs before workflow
    polling or publication.
  - Add compatibility fixtures proving existing switchover checkpoints reload
    with its unchanged effective encoded ceiling, native remove selects limits
    from its contract version, and a changed limit without a version bump is
    rejected by contract-consistency tests.
- **`durable-execution/tests/support/scenarios.rs`**:
  - Update shared fixtures/builders for separate active and terminal limits and
    retain admitted-capacity projection coverage.
- **`durable-execution/tests/kubernetes_checkpoint.rs`** and
  **`durable-execution/tests/kubernetes_checkpoint_real.rs`**:
  - Update limit construction and retain malformed, owner, CAS, and real
    provider behavior.
- **`kuberic-operator/src/durable/pilot_store.rs` → `kuberic-operator/src/durable/checkpoint_store.rs`**:
  - Rename the shared measured store to production-neutral terminology without
    changing provider or accepted-write accounting behavior.
- **`kuberic-operator/src/durable/workflow_host.rs`**:
  - Consume the production-neutral checkpoint store and expose the host,
    measurement, and one-use permit services required by the runner.
- **`kuberic-operator/src/durable/runner.rs`**:
  - Add the shared bounded lifecycle runner, a common outcome vocabulary, and
    an operation adapter interface for the six operation-specific
    responsibilities.
  - Keep reconcile as the scheduler and return bounded wait/requeue or validated
    terminal handoff results; do not publish topology in the generic layer.
  - Add fake-adapter tests for active, terminal, rejected, isolated,
    incompatible, conflicted, unknown-write, persistence-failure,
    nondeterministic, one-use permit, bounded-fuel, and deadline-clamped
    outcomes.
- **`kuberic-operator/src/durable/mod.rs`**:
  - Export `checkpoint_store`, `runner`, and existing host services under
    production-neutral names while legacy workflows are still present.

### Success Criteria

#### Automated Verification

- [ ] Kernel checkpoint tests pass:
  `cargo test -p kuberic-durable-execution --tests`
- [ ] Shared runner and store tests pass:
  `cargo test -p kuberic-operator --features durable-switchover-pilot,durable-remove-replica-pilot durable_runner`
- [ ] Measurement store tests pass:
  `cargo test -p kuberic-operator --features durable-switchover-pilot,durable-remove-replica-pilot checkpoint_store`
- [ ] Formatting passes: `cargo fmt --all -- --check`

#### Manual Verification

- [ ] The runner interface contains no remove, switchover, Kubernetes effect, or
  topology-publication decisions.
- [ ] A terminal envelope between 12,289 and the active ceiling is rejected as
  terminal while an active envelope at its admitted ceiling remains valid.
- [ ] Existing conflict and outcome-unknown paths still require authoritative
  reload before another dispatch permit can exist.

### Local Commit

`[Framework-Native Remove Replica] Add shared durable runner infrastructure`

---

## Phase 2: Switchover Runner Adoption

### Changes Required

- **`kuberic-operator/src/durable/pilot.rs`**:
  - Implement the shared runner adapter for switchover observation collection,
    prepared-effect authority, dispatch/quarantine interpretation, deadline,
    terminal validation, and publication handoff.
  - Preserve current workflow/activity/terminal contracts and measurement
    semantics.
- **`kuberic-operator/src/durable/runner.rs`**:
  - Exercise the production adapter path with switchover and expose only the
    additional workflow-neutral hooks proven necessary by the migration.
- **`kuberic-operator/src/reconciler.rs`**:
  - Replace the switchover-specific host-outcome loop with one invocation of the
    shared runner plus the existing switchover terminal publisher/condition
    mapping.
  - Remove the duplicated load, permit, quarantine, reload, store failure,
    nondeterminism, and requeue branches for switchover.
- **`examples/kvstore/tests/reconciler.rs`**:
  - Keep the existing switchover happy-path, restart, conflict, ambiguous-write,
    quarantine, terminal reload, and measurement assertions running through the
    common runner.
  - Add a named switchover common-outcome matrix covering every FR-017 outcome
    that is reachable for the operation: active, terminal, incompatible,
    rejected, isolated, conflict reload, unknown-write reload,
    persistence-failure, and nondeterminism. Where an outcome is intentionally
    impossible for the switchover contract, assert and document that fact.
  - Add named tests covering all six switchover FR-019 responsibilities:
    observation collection, authority/preparation, exact effect dispatch,
    deadline policy, terminal validation, and publication.

### Success Criteria

#### Automated Verification

- [ ] Switchover unit/replay matrix passes:
  `cargo test -p kuberic-operator --features durable-switchover-pilot pilot`
- [ ] Switchover measurement gates pass:
  `cargo test -p kuberic-operator --features durable-switchover-pilot measurements_ -- --nocapture`
- [ ] Switchover reconciler matrix passes:
  `cargo test -p kvstore --test reconciler test_durable_execution_switchover_pilot_`
- [ ] Add/failover regression subset passes:
  `cargo test -p kvstore --test reconciler test_durable_add && cargo test -p kvstore --test reconciler test_durable_failover`

#### Manual Verification

- [ ] `kuberic-operator/src/reconciler.rs` has one durable host lifecycle loop,
  with switchover-specific evidence, effects, terminal checks, and publication
  still visible at the adapter boundary.
- [ ] Same-cycle progression remains allowed only when the recorded observation
  contains all authority needed for the next transition.
- [ ] Switchover's public pilot selector and behavior are unchanged.

### Local Commit

`[Framework-Native Remove Replica] Run switchover through shared recovery`

---

## Phase 3: Compact Native Remove Contract

### Changes Required

- **`kuberic-operator/src/durable/remove_replica_execution.rs`**:
  - Add the production remove contract with a new explicit contract version and
    `deny_unknown_fields` on all persisted structures.
  - Define the compact immutable admission input and derive the reduced
    topology from the previous snapshot plus exact target identity.
  - Define compact tagged boundary inputs/results for passive observations,
    exact replica commands, exact UID label/delete commands, typed evidence,
    effect outcomes, and bounded proven-no-admission redelivery.
  - Reconstruct transient `DurableOperationStatus` only in memory when calling
    the existing remove domain decision/validation functions.
  - Define completed, compensated, unsafe, rejected, and
    `IncompatibleContract` terminals without duplicate commit-authority
    structures.
  - Declare and enforce: 3 replicas, 16 records, 4,096-byte boundary inputs,
    2,048-byte results, 262,144-byte active records, 12,288-byte terminal
    records, 4,096-byte terminal payloads, and bounded transition fuel.
  - Project maximum retry/fault history at admission and reject capacity
    failures before checkpoint creation or effect exposure.
  - Add deterministic replay, transition monotonicity, exact prepared-command,
    terminal validation, malformed/unknown field, unsupported version, and all
    six one-byte-over boundary tests.
- **`kuberic-operator/src/durable/remove_replica.rs`**:
  - Keep the existing remove decision engine and authority validation as the
    operation-specific domain component.
  - Expose only the domain helpers needed by the compact workflow; do not copy
    authority logic into the runner.
- **`kuberic-operator/src/durable/effects.rs`**:
  - Reuse exact signed replica, label, and delete commands and ensure their
    production encodings fit the 4,096-byte input contract.
- **`kuberic-operator/src/crd.rs`**:
  - Add structured `RemoveReplicaExecutionStatus` and compact immutable input
    schema types for later production routing, without exposing a third
    operator-selectable mode.
- **`kuberic-operator/src/durable/checkpoint_store.rs`**:
  - Add native remove measurement decoding for boundary class, terminal
    accounting, accepted writes, and active/terminal byte ranges.
- **`kuberic-operator/src/durable/mod.rs`**:
  - Register the native module alongside the still-present legacy module until
    the safety deletion gate is complete.

### Success Criteria

#### Automated Verification

- [ ] Compact contract and replay tests pass:
  `cargo test -p kuberic-operator --features durable-remove-replica-pilot remove_replica_execution`
- [ ] Exact command adapter tests pass:
  `cargo test -p kuberic-operator --features durable-switchover-pilot,durable-remove-replica-pilot durable_effect_outcome`
- [ ] Kernel typed-bound tests pass:
  `cargo test -p kuberic-durable-execution typed_input_and_result_bounds_fail_before_persistence`
- [ ] Maximum projected 16-record fault history fits 262,144 bytes and the
  maximum terminal fits 12,288 bytes in a named admission test.

#### Manual Verification

- [ ] No compact activity input or result contains
  `DurableOperationStatus`, the complete mutable remove state, or both full
  previous and reduced configuration descriptors.
- [ ] Completed terminal authority is validated against immutable input and
  commit evidence rather than serialized twice.
- [ ] Unsupported version and legacy-shape fixtures return
  `IncompatibleContract`; malformed and oversized fixtures remain distinct
  rejection/isolation cases.

### Local Commit

`[Framework-Native Remove Replica] Add compact native remove contract`

---

## Phase 4: Native Integration and Safety Traceability

### Changes Required

- **`kuberic-operator/src/durable/remove_replica_execution.rs`**:
  - Implement the remove adapter for the shared runner: exact evidence
    collection requirements, prepared-effect validation, one-use dispatch,
    quarantine resolution, deadline selection, terminal validation, and
    publication handoff.
  - Preserve fresh-authority stops before any command that depends on changed
    epoch, incarnation, UID, role, control version, generation, or
    configuration evidence.
- **`kuberic-operator/src/reconciler.rs`**:
  - Add native remove acceptance/recovery entry points behind a test-only
    harness and route them through the shared runner while legacy production
    selection remains available until the deletion gate. No public field,
    feature, or production branch may select this path before Phase 5, so Phase
    4 cannot introduce a third production execution mode.
  - Preserve terminal-write-and-reload-before-status/topology publication,
    exact connection/retirement cleanup, UID-fenced label/delete behavior, and
    owner-bound checkpoint construction.
  - Add standardized typed conditions for wait, incompatible, rejected,
    isolated, persistence failure, unsafe, compensated, and completed outcomes.
- **`examples/kvstore/tests/reconciler.rs`**:
  - Migrate and rename the pilot reconciler matrix to
    `test_framework_native_remove_replica_*`.
  - Port explicit-only end-to-end assertions for exact primary-status gaps,
    exact target-status gaps, replacement UID cleanup, post-commit primary
    restart, malformed agent status, and publication/status conflicts.
  - Preserve every-boundary restart, lost reply, proven-no-admission,
    conflict, outcome-unknown, stale UID/incarnation/generation, Force
    authority, repeated identity, terminal reload, publication ordering, and
    post-commit ambiguity fixtures.
  - Add a named native-remove common-outcome matrix covering every applicable
    FR-017 outcome: active, terminal, incompatible, rejected, isolated,
    conflict reload, unknown-write reload, persistence-failure, and
    nondeterminism. If the native contract makes an outcome unreachable, assert
    and document that fact.
  - Add named tests covering all six remove FR-019 responsibilities:
    observation collection, authority/preparation, exact effect dispatch,
    deadline policy, terminal validation, and publication.
- **`kuberic-tests/src/kvstore_k8s.rs`**:
  - Add `test_kvstore_k8s_framework_native_remove_replica`, which starts from a
    healthy three-member set, requests ScaleDown without a mode selector, waits
    for a healthy two-member topology, and verifies the native terminal
    reference/publication ordering exposed by the API.
- **`kuberic-tests/src/lib.rs`**:
  - Add only the reusable wait/patch helpers required by the live remove test.
- **`.paw/work/framework-native-remove-replica/SafetyTraceability.md`**:
  - Create the deletion-gate matrix mapping every SC-002 inventory item to at
    least one exact passing native/shared replacement test name.
  - Record whether each assertion is native workflow, shared runner/kernel,
    Kubernetes provider, or live-cluster coverage. No row may reference only a
    test scheduled for deletion.
- **`durable-execution/tests/kubernetes_checkpoint_real.rs`**:
  - Run the existing exact-owner garbage-collection real-API test against the
    isolated Kuberic `kind` cluster before the deletion gate.
- **`durable-execution/tests/feasibility.rs`**:
  - Retain and run the cleanup-role authorization test proving the writer
    cannot delete retained records and the distinct cleanup identity can.

### Success Criteria

#### Automated Verification

- [ ] Native remove unit/runner matrix passes:
  `cargo test -p kuberic-operator --features durable-remove-replica-pilot framework_native_remove_replica`
- [ ] Native reconciler matrix passes:
  `cargo test -p kvstore --features durable-remove-replica-pilot --test reconciler test_framework_native_remove_replica_`
- [ ] Shared owner/provider tests pass:
  `cargo test -p kuberic-durable-execution --features kubernetes --test kubernetes_checkpoint`
- [ ] Real owner-GC coverage passes before deletion with the isolated config:
  `KUBECONFIG="$HOME/.kube/kuberic-kind-config" cargo test -p kuberic-durable-execution --features kubernetes --test kubernetes_checkpoint_real validates_real_api_cas_watch_compaction_and_ambiguous_recovery -- --nocapture`
- [ ] Retained-cleanup authorization coverage passes:
  `cargo test -p kuberic-durable-execution --test feasibility checkpoint_rbac_examples_are_structural_and_lifecycle_specific`
- [ ] Existing explicit remove regression suite still passes before deletion:
  `cargo test -p kvstore --test reconciler test_durable_remove && cargo test -p kvstore --test reconciler test_durable_force_remove && cargo test -p kvstore --test reconciler test_remove_`
- [ ] Every row in `SafetyTraceability.md` names at least one test that passed
  in this phase.
- [ ] The switchover and native-remove common-outcome matrices cover every
  applicable FR-017 outcome and assert any contract-impossible outcome as
  unreachable; each operation has named passing tests for all six FR-019
  responsibilities.

#### Manual Verification

- [ ] The SC-002 matrix contains all 34 listed invariants with no unmapped,
  ambiguous, or legacy-only row.
- [ ] Native status-gap fixtures prove repeated reconcile waits without
  operation-generation or status-write churn.
- [ ] Terminal publication cannot occur until a terminal checkpoint has been
  accepted and reloaded.
- [ ] No uncertain exposed effect can obtain a second dispatch permit until
  authoritative observation proves no admission.

### Local Commit

`[Framework-Native Remove Replica] Prove native remove safety coverage`

---

## Phase 5: Production Graduation and Legacy Removal

### Entry Gate

Phase 4's complete `SafetyTraceability.md` matrix and all referenced tests must
pass before any explicit remove orchestration or legacy test is deleted.

### Changes Required

- **`kuberic-operator/src/crd.rs`**:
  - Remove `remove_replica_execution_mode`,
    `RemoveReplicaExecutionMode`, and public
    `durable_remove_replica_pilot`/`DurableRemoveReplicaPilotStatus`.
  - Expose only `status.removeReplicaExecution` for native remove identity.
  - Accept the removed legacy pilot field for deserialization only, then
    atomically persist a production `removeReplicaExecution` incompatibility
    marker before the next status replacement can discard the legacy field.
    The marker remains serialized across all later status writes and contains
    enough stable identity to prove the same legacy execution remains blocked.
  - Update CRD serialization/schema tests to prove the selector and pilot names
    are absent and the compact production reference is present.
- **`kuberic-operator/src/reconciler.rs`**:
  - Make native acceptance unconditional for eligible remove requests.
  - Remove explicit-versus-pilot selection, feature rejection, dual recovery
    routing, pilot conditions, process-summary comparison telemetry, and
    duplicate host cleanup paths.
  - Map a legacy pilot value or a legacy
    `status.operation.kind == RemoveReplica` to the durable production
    incompatibility marker in one status update, then return typed
    incompatibility on every later reconcile without creating a native
    execution.
  - Delete the explicit remove action loop only after the entry gate.
- **`kuberic-operator/src/durable/remove_replica_pilot.rs`**:
  - Delete the pilot module after all reusable tests and logic have moved to
    `remove_replica_execution.rs`.
- **`kuberic-operator/src/durable/remove_replica.rs`**:
  - Remove explicit CRD-persistence/reconcile-only helpers and tests while
    retaining the pure remove domain decision, validation, and authority
    components used by native execution.
- **`kuberic-operator/src/durable/mod.rs`**:
  - Export native remove, shared runner, host, and checkpoint store by default;
    remove all remove-pilot feature gates.
- **`kuberic-operator/src/main.rs`**:
  - Construct the durable runtime in the normal production binary because
    remove-replica always depends on it; keep switchover's optional adapter
    wiring intact.
- **`kuberic-operator/Cargo.toml`**:
  - Remove `durable-remove-replica-pilot`.
  - Make the durable kernel/Kubernetes provider and runtime randomness
    dependencies part of the default operator build; retain only the
    switchover pilot feature for switchover selection.
- **`examples/kvstore/Cargo.toml`**:
  - Remove the forwarded remove-pilot feature.
- **`kuberic-operator/deploy/deployment.yaml`**:
  - Regenerate/update the CRD schema to remove
    `removeReplicaExecutionMode` and `durableRemoveReplicaPilot`, add the
    compact `removeReplicaExecution` status shape, and preserve checkpoint
    writer/cleanup RBAC semantics.
- **`examples/kvstore/deploy/kubericset.yaml`**:
  - Continue to request ordinary three-member operation with no remove mode;
    use it as the default-native schema fixture.
- **`examples/kvstore/tests/reconciler.rs`**:
  - Delete pilot-selection/default-explicit/unsupported-feature tests and
    explicit implementation fixtures only after their native replacements are
    named in the traceability matrix.
  - Add clean-break tests for legacy pilot status, unsupported native versions,
    and legacy explicit remove records. Each test must reconcile through at
    least two full status replacements and a simulated controller restart,
    proving that the serialized production incompatibility marker persists and
    no fresh execution is admitted.

### Success Criteria

#### Automated Verification

- [ ] Default operator native tests pass without a feature:
  `cargo test -p kuberic-operator framework_native_remove_replica`
- [ ] Default kvstore reconciler tests pass without a feature:
  `cargo test -p kvstore --test reconciler test_framework_native_remove_replica_`
- [ ] CRD/deployment consistency tests pass:
  `cargo test -p kuberic-operator framework_native_remove_schema`
- [ ] Default build surface passes: `cargo check --all-targets`
- [ ] Switchover optional feature still passes:
  `cargo test -p kuberic-operator --features durable-switchover-pilot pilot`
- [ ] Repository search finds no remove pilot selector/build/status surface:
  `rg 'removeReplicaExecutionMode|durableRemoveReplicaPilot|durable-remove-replica-pilot|DurableRemoveReplicaPilot' --glob '!target/**'`
  returns no matches outside historical PAW research artifacts.

#### Manual Verification

- [ ] A new remove request has exactly one persisted execution reference and
  cannot select another engine.
- [ ] Old pilot and explicit records fail closed with typed incompatibility and
  remain distinguishable from absent/fresh execution across repeated status
  writes and controller restart.
- [ ] `status.operation` continues serving create, add, failover, and any
  unaffected explicit switchover behavior.
- [ ] The production binary has no second remove reconciliation loop.

### Local Commit

`[Framework-Native Remove Replica] Graduate native remove to production`

---

## Phase 6: Measurements and Full Validation

### Changes Required

- **`examples/kvstore/tests/reconciler.rs`**:
  - Rename the canonical fixture to
    `test_framework_native_remove_replica_three_no_fault_measurement_samples`.
  - Run at least three no-fault three-member ScaleDown samples and assert per
    sample: 3 external effects, 2 passive observations, 5 durable boundaries,
    6 accepted writes, and maximum active encoded record no larger than 49,152
    bytes.
  - Report per-run active minimum/maximum, terminal record bytes, and terminal
    payload bytes separately; do not freeze run-specific byte values below the
    contractual ceilings.
- **`kuberic-operator/src/durable/remove_replica_execution.rs`**:
  - Keep named one-byte-over admission fixtures for all six count/byte limits
    and a maximum theoretical retry/fault history fixture.
- **`kuberic-operator/src/durable/checkpoint_store.rs`**:
  - Verify accepted-write attribution and active/terminal record
    classification against the native contract.
- **`scripts/measure-switchover-complexity.py`**:
  - Remove remove-specific explicit-versus-pilot marginal/amortization scopes,
    ratios, and comparison classification.
  - Retain genuinely general or switchover-specific measurement behavior under
    accurate labels.
- **`scripts/tests/test_measure_switchover_complexity.py`**:
  - Remove closed remove-comparison threshold tests and update registry/output
    assertions for the remaining supported measurements.
- **`.github/workflows/CI.yml`**:
  - Replace feature-gated remove pilot commands with default native unit,
    shared-runner, reconciler, compatibility, bounds, and three-sample
    measurement gates.
  - Preserve format, clippy, build, switchover, all-feature, image, and Kind
    jobs.
- **`.paw/work/framework-native-remove-replica/SafetyTraceability.md`**:
  - Record the final command results for the deletion-gate tests and verify no
    mapped test was removed during graduation.

### Success Criteria

#### Automated Verification

- [ ] Representative measurement passes:
  `cargo test -p kvstore --test reconciler test_framework_native_remove_replica_three_no_fault_measurement_samples -- --nocapture`
- [ ] Bounds/admission matrix passes:
  `cargo test -p kuberic-operator framework_native_remove_replica`
- [ ] Complexity tooling passes:
  `python3 scripts/measure-switchover-complexity.py && python3 -m unittest scripts.tests.test_measure_switchover_complexity`
- [ ] Type check passes: `cargo check --all-targets`
- [ ] Formatting passes: `cargo fmt --all -- --check`
- [ ] Clippy passes: `cargo clippy --all-targets -- -D warnings`
- [ ] Build passes: `cargo build --all-targets`
- [ ] Targeted component suite passes:
  `cargo test -p kuberic-core -p kuberic-operator -p kvstore -p sqlite-replicated`
- [ ] Documentation tests pass: `cargo test --doc --workspace`
- [ ] Full workspace/all-feature suite passes:
  `cargo test --all --all-features`

#### Isolated Kind Verification

- [ ] Probe only the isolated kubeconfig with
  `KUBECONFIG="$HOME/.kube/kuberic-kind-config" kubectl --kubeconfig "$HOME/.kube/kuberic-kind-config" cluster-info`.
  If that exact config is absent or unhealthy, create only cluster `kind` with
  `KUBECONFIG="$HOME/.kube/kuberic-kind-config" kind create cluster --name kind --config deploy/kind-config.yaml --kubeconfig "$HOME/.kube/kuberic-kind-config"`.
  Do not enumerate Kind clusters and never read, switch to, modify, or delete
  `capi-kamaji-management` or `capi-worker-spike`.
- [ ] Build/load repository images with `just images` while
  `KUBECONFIG="$HOME/.kube/kuberic-kind-config"`; the repository recipe targets
  cluster name `kind`.
- [ ] Real provider/owner-GC tests pass against that cluster:
  `KUBECONFIG="$HOME/.kube/kuberic-kind-config" cargo test -p kuberic-durable-execution --features kubernetes --test kubernetes_checkpoint_real -- --nocapture`
- [ ] Existing live smoke tests pass before mutation:
  `KUBECONFIG="$HOME/.kube/kuberic-kind-config" cargo test -p kuberic-tests test_kvstore_k8s_status_healthy -- --nocapture`
  and
  `KUBECONFIG="$HOME/.kube/kuberic-kind-config" cargo test -p kuberic-tests test_kvstore_k8s_write_read -- --nocapture`.
- [ ] Live native removal passes:
  `KUBECONFIG="$HOME/.kube/kuberic-kind-config" cargo test -p kuberic-tests test_kvstore_k8s_framework_native_remove_replica -- --nocapture`.
- [ ] Inspect the live object and checkpoint to confirm no mode selector is
  required, the admitted owner UID is retained, terminal durability precedes
  the published two-member topology, and no replacement UID is mutated. Every
  `kubectl` invocation uses both
  `KUBECONFIG="$HOME/.kube/kuberic-kind-config"` and
  `--kubeconfig "$HOME/.kube/kuberic-kind-config"`.
- [ ] Leave the dedicated `kind` cluster and isolated kubeconfig intact after
  validation unless this workflow created the cluster and cleanup is
  intentionally required; never clean up any CAPI cluster.

#### Manual Verification

- [ ] All three representative samples are at or below 49,152 active bytes and
  preserve the exact semantic/write counts.
- [ ] Theoretical fault capacity remains explicitly bounded at 262,144 active
  bytes even though the no-fault representative gate is 49,152 bytes.
- [ ] CI contains no remove comparison or pilot feature gate.
- [ ] The safety traceability matrix still maps every SC-002 row to a passing,
  retained test.

### Local Commit

`[Framework-Native Remove Replica] Update measurements and validation gates`

---

## Phase 7: Documentation

### Changes Required

- **`.paw/work/framework-native-remove-replica/Docs.md`**:
  - Create the as-built technical reference using `paw-docs-guidance`.
  - Record the shared runner contract, six operation-specific responsibilities,
    compact remove contract/version, compatibility behavior, safety
    traceability, count/byte limits, final measurements, validation commands,
    and owner/retention behavior.
- **`durable-execution/README.md`**:
  - Replace the closed remove pilot comparison with the production native
    consumer, independent active/terminal bounds, and retained owner/cleanup
    model.
- **`docs/features/kuberic/durable-execution-roadmap.md`**:
  - Mark remove-replica as graduated, remove explicit-default and negative
    amortization language, and report final representative measurements with
    lifecycle constants separated from run-specific byte values.
- **`docs/features/kuberic/testing.md`**:
  - Replace feature-gated pilot commands with default native commands, the
    bounds matrix, safety traceability, three-sample measurement gate, and
    isolated Kind procedure.
- **`docs/features/kuberic/protocols.md`**:
  - Remove build/spec mode selection and describe the single framework-native
    flow, explicit compatibility rejection, and unchanged ReplicaAgent
    protocol.
- **`docs/features/kuberic/operator.md`**:
  - Document the shared in-process runner, checkpoint owner/retention model,
    terminal-before-publication rule, and absence of an extra worker or
    scheduler.
- **`docs/features/kuberic/implemented/agent-owned-replica-remove-protocol.md`**:
  - Record the production execution contract, compact durable boundaries,
    authority/cleanup invariants, and clean-break compatibility policy.
- **`kuberic-operator/README.md`**:
  - Add the default native remove behavior and remove any implication that
    durable execution is only an optional switchover experiment.
- **`.paw/work/framework-native-remove-replica/SafetyTraceability.md`**:
  - Reconcile the implementation-time matrix with final retained test names;
    link or reproduce the final matrix in `Docs.md`.

### Success Criteria

#### Automated Verification

- [ ] Rust documentation tests pass: `cargo test --doc --workspace`
- [ ] Documentation command examples match passing Phase 6 commands.
- [ ] Repository search finds no current remove pilot/mode terminology outside
  historical PAW research:
  `rg 'remove-replica pilot|removeReplicaExecutionMode|durableRemoveReplicaPilot|durable-remove-replica-pilot' README.md durable-execution docs kuberic-operator examples .github scripts`

#### Manual Verification

- [ ] Documentation names every common lifecycle outcome from FR-017 and all
  six operation-specific responsibilities from FR-019.
- [ ] Documentation explicitly states that add-replica is not migrated and
  explains how a future adapter would supply its observation, authority/effect,
  deadline, terminal-validation, and publication behavior.
- [ ] Measurements distinguish external effects, passive observations,
  boundaries, accepted writes, active record size, terminal record size, and
  terminal payload size.
- [ ] Compatibility, owner garbage collection, separately authorized orphan
  cleanup, and no-extra-service architecture are described accurately.

### Local Commit

`[Framework-Native Remove Replica] Document production native removal`

---

## Final Validation and Completion Gate

- [ ] All seven phase commits are on
  `feature/framework-native-remove-replica`; no phase/planning/docs branch or
  intermediate PR exists.
- [ ] The complete Phase 6 command set and isolated Kind sequence pass after the
  documentation changes.
- [ ] `git diff main...HEAD` contains no replica-agent or gRPC semantic change.
- [ ] `git diff main...HEAD` contains no add-replica migration.
- [ ] Exactly one final PR will be created from
  `feature/framework-native-remove-replica` to `main` only after the mandatory
  final pre-PR milestone and final single-model review.

## References

- Issue: none
- Workflow context:
  `.paw/work/framework-native-remove-replica/WorkflowContext.md`
- Spec: `.paw/work/framework-native-remove-replica/Spec.md`
- Research:
  `.paw/work/framework-native-remove-replica/SpecResearch.md`
- Code research:
  `.paw/work/framework-native-remove-replica/CodeResearch.md`
