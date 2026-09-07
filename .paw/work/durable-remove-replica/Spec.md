# Feature Specification: Durable Remove Replica

**Branch**: `feature/durable-remove-replica`  |  **Created**: 2026-09-07  |  **Status**: Draft
**Input Brief**: Add an opt-in kernel-hosted remove-replica workflow to test whether the durable-execution framework amortizes across a second substantial workflow.

## Overview

Kuberic operators need evidence that the experimental durable-execution kernel
reduces the marginal complexity of implementing additional topology workflows,
not merely that it can host one switchover pilot. Remove-replica is the strongest
available second test because its existing explicit implementation is the
largest hand-written durable workflow and exercises irreversible topology
commit, exact identity fencing, uncertain outcomes, cleanup, and final status
publication.

This work adds a separately selectable, default-off durable remove-replica path
while preserving the existing explicit workflow as the default. The new path
must reuse the capabilities already shared by the first durable workflow rather
than recreate equivalent workflow-specific machinery. It must preserve the
safety semantics of the existing remove-replica protocol and provide measured
evidence showing whether the shared infrastructure actually amortizes across
two workflows.

The work also repairs a feasibility test that currently mistakes a test-only
async runtime dependency for a library runtime dependency. Documentation will
report the classifier's recomputed result and reconcile stale post-switchover
claims with the measured second-workflow outcome. A negative amortization result
is acceptable and must be reported without changing the evidence or redefining
measurement scopes to manufacture a favorable conclusion.

## Objectives

- Provide an explicitly selected kernel-hosted remove-replica workflow without
  changing the default explicit behavior.
- Preserve remove-replica safety, determinism, fencing, conservative ambiguity
  handling, bounded persistence, and terminal-before-status ordering.
- Reuse capabilities already shared by the first durable workflow, generalizing
  common behavior only where required by the second workflow.
- Quantify the second workflow's body, comparable legacy scope, marginal
  integration cost, shared-infrastructure growth, checkpoint writes, durable
  boundaries, and checkpoint sizes using non-overlapping scopes.
- Correct the runtime-neutrality feasibility predicate without weakening its
  ability to detect a real library runtime dependency.
- Update durable-execution documentation with evidence-backed current
  boundaries and conclusions.

## User Scenarios & Testing

### User Story P1 – Opt In to Durable Remove Replica

Narrative: As a Kuberic operator evaluating the durable-execution kernel, I can
explicitly select a kernel-hosted remove-replica operation while deployments
that do not opt in continue to use the established explicit workflow.

Independent Test: Build with the dedicated feature, opt one eligible
remove-replica operation into the durable path, and verify that an otherwise
equivalent default configuration still starts the explicit path.

Acceptance Scenarios:

1. Given a default build or a set that does not opt in, when removal is
   required, then the existing explicit remove-replica workflow is selected.
2. Given a build with the dedicated default-off feature and an explicit durable
   selection, when an eligible removal is required, then the kernel-hosted
   workflow is selected and checkpointed.
3. Given a durable selection without compiled feature support, when
   reconciliation begins, then the operator rejects the unsupported selection
   rather than silently falling back.
4. Given either selection, when another topology workflow is requested, then
   its existing selection and behavior remain unchanged.

### User Story P1 – Preserve Safe Removal Semantics

Narrative: As an operator removing a secondary replica, I receive the same
safety guarantees from the durable path as from the explicit path, including
exact authority, identity and epoch fences, conservative handling of uncertain
effects, and irreversible-commit ordering.

Independent Test: Drive the durable path through success, restart, redelivery,
identity drift, lost-reply, conflict, timeout, and unsafe-ambiguity scenarios
and compare its observable commands and terminal disposition with the explicit
contract.

Acceptance Scenarios:

1. Given a valid target and retained quorum, when removal succeeds, then the
   exact reduced topology is committed before cleanup and published only after
   the required retirement and deletion evidence exists.
2. Given stale primary, target, UID, generation, epoch, protocol, or
   configuration evidence, when the next effect is prepared, then no unfenced
   command is dispatched.
3. Given an unknown dispatch or persistence outcome, when reconciliation
   resumes, then authoritative state is reloaded and no duplicate permit is
   inferred.
4. Given ambiguity after the irreversible commit boundary, when safety cannot
   be proven, then the execution is quarantined or poisoned rather than rolled
   back or guessed complete.
5. Given a restart at any durable boundary, when the execution resumes, then
   replay produces the same prepared command or terminal outcome.

### User Story P1 – Measure Amortization Honestly

Narrative: As a maintainer deciding whether to expand kernel adoption, I can
compare the second workflow's marginal cost with the first pilot and its legacy
implementation using reproducible, non-overlapping measurements.

Independent Test: Run the repository complexity measurement and the
representative successful remove-replica measurement and verify that every
reported scope, count, and byte range is reproducible from the tested build.

Acceptance Scenarios:

1. Given the completed implementation, when complexity measurement runs, then
   it reports the remove-replica workflow body, its comparable legacy scope,
   second-workflow integration/marginal scope, shared reusable infrastructure,
   shared-scope growth, and combined totals.
2. Given configured charged scopes, when any two overlap improperly, then the
   measurement fails rather than double-counting or silently accepting overlap.
3. Given a representative successful execution, when measurement completes,
   then it reports external effects, passive observations, durable boundaries,
   accepted checkpoint writes, and active and terminal checkpoint byte ranges.
4. Given the measured executable-line and decision-point results, when the
   classification rules below are applied, then the conclusion reports a
   positive, negative, or inconclusive result without changing scope attribution.

### User Story P2 – Recompute Feasibility Correctly

Narrative: As a kernel maintainer, I receive a feasibility classification based
on runtime dependencies used by the library rather than dependencies used only
by tests.

Independent Test: Run the feasibility evidence test and verify that a test-only
runtime is permitted while an equivalent library runtime dependency is rejected.

Acceptance Scenarios:

1. Given the current manifest with a runtime only in development dependencies,
   when runtime neutrality is evaluated, then the library is not classified as
   runtime-bound for that reason.
2. Given a fixture with a known async runtime in library dependencies, when the
   same predicate is evaluated, then runtime neutrality fails.
3. Given the corrected predicate, when the complete feasibility classifier
   runs, then documentation reports its actual result and actual failing
   predicates without a preselected outcome.

### User Story P2 – Maintain Accurate Durable-Execution Documentation

Narrative: As a maintainer or evaluator, I can understand which workflows are
kernel-hosted, which paths remain experimental, which contracts are stable, and
what the latest measurements do and do not prove.

Independent Test: Compare the README and roadmap claims with executable tests,
feature defaults, measured outputs, and current workflow routing.

Acceptance Scenarios:

1. Given the completed port, when documentation is read, then it identifies
   switchover and remove-replica as explicitly gated kernel-hosted workflows and
   confirms that explicit paths remain the defaults.
2. Given post-prepared-exposure measurements, when feasibility rationale is
   described, then no removed write-efficiency exception is cited as current.
3. Given run-specific checkpoint sizes, when they are documented, then they are
   ranges or snapshots compared with stable ceilings, not compatibility
   promises.
4. Given measured complexity results, when the roadmap states an amortization
   conclusion, then it cites the reported marginal and shared-scope evidence.

### Edge Cases

- A durable selection is present but the matching Cargo feature is absent.
- The target disappears, is replaced under a new UID/incarnation, or changes
  generation before or after dispatch.
- The primary changes generation or loses exact authority while an execution is
  active.
- A dispatch succeeds but its response is lost.
- A checkpoint write has an unknown outcome or encounters compare-and-swap
  conflict.
- A process restarts immediately before dispatch, after dispatch exposure,
  after observation, after irreversible commit, during cleanup, or after
  terminal persistence but before status publication.
- The active checkpoint would exceed its admitted encoded-size budget or a
  terminal payload would exceed its terminal bound.
- A removal request exceeds the workflow's admitted replica or activity bounds.
- Complexity markers are missing, nested incorrectly, or overlap another
  charged scope.
- Shared infrastructure growth makes amortization negative or inconclusive.

## Requirements

### Functional Requirements

- FR-001: The operator MUST retain the existing explicit remove-replica path as
  the default behavior. (Stories: P1 Opt In)
- FR-002: The kernel-hosted remove-replica path MUST require a dedicated,
  explicit, default-off build feature and runtime selection. (Stories: P1 Opt In)
- FR-003: An unsupported durable selection MUST fail explicitly and MUST NOT
  silently fall back to the explicit path. (Stories: P1 Opt In)
- FR-004: The durable workflow MUST satisfy every item in the Safety Parity
  Inventory below for both successful and interrupted executions.
  (Stories: P1 Safety)
- FR-005: An external effect MUST NOT become dispatchable until its exact
  identity and maximum result capacity are durably accepted, and one acceptance
  MUST authorize at most one dispatch attempt before authoritative reload.
  (Stories: P1 Safety)
- FR-006: Replay MUST deterministically reproduce accepted activities,
  prepared commands, observations, and terminal outcomes. (Stories: P1 Safety)
- FR-007: Unknown effect or checkpoint outcomes MUST trigger conservative reload
  or quarantine behavior and MUST NOT authorize duplicate dispatch, inferred
  commit, unsafe rollback, cleanup, or completion. (Stories: P1 Safety)
- FR-008: The workflow MUST preserve exact UID, incarnation, generation, epoch,
  protocol, configuration, and quorum fences required by the explicit contract.
  (Stories: P1 Safety)
- FR-009: Commit evidence MUST be durable before cleanup begins, and a successful
  terminal checkpoint MUST be accepted before final topology/status publication.
  (Stories: P1 Safety)
- FR-010: Active and terminal checkpoints MUST satisfy bounded admission,
  activity-result reservation, encoded checkpoint, and terminal-payload limits.
  (Stories: P1 Safety, P1 Measurement)
- FR-011: A capability already charged to shared reusable infrastructure for the
  switchover pilot MUST remain shared when serving remove-replica; any extension
  MUST be charged to shared growth or second-workflow integration according to
  its actual reuse boundary. (Stories: P1 Measurement)
- FR-012: Every existing switchover replay, effect, checkpoint, restart, and
  reconciler test that passes before the change MUST continue to pass after
  shared generalization. (Stories: P1 Opt In, P1 Measurement)
- FR-013: Complexity tooling MUST report non-overlapping charged scopes for both
  workflow bodies, comparable explicit scopes, shared reusable infrastructure,
  workflow-specific integration, shared-scope growth, marginal cost, and
  combined cost. (Stories: P1 Measurement)
- FR-014: Complexity tooling MUST reject invalid overlapping charged scopes.
  (Stories: P1 Measurement)
- FR-015: Three successful no-fault samples of the same admitted three-member
  ScaleDown removal, reducing one exact secondary to a stable two-member
  topology, MUST report external effects, passive observations, durable
  boundaries, accepted writes, and the minimum-to-maximum active and terminal
  checkpoint sizes against stable ceilings. (Stories: P1 Measurement)
- FR-016: The feasibility runtime-neutrality predicate MUST distinguish library
  dependencies from development dependencies and MUST reject a real async
  runtime dependency in the library dependency scope. (Stories: P2 Feasibility)
- FR-017: The full feasibility classifier MUST be rerun after the predicate
  correction, and documentation MUST reflect its actual classification and
  reason. (Stories: P2 Feasibility, P2 Documentation)
- FR-018: README and roadmap documentation MUST remove stale write-efficiency
  exception language and state the measured second-workflow amortization result
  without overstating run-specific evidence. (Stories: P1 Measurement,
  P2 Documentation)
- FR-019: The Validation Coverage Matrix below MUST pass for add-replica,
  explicit remove-replica, durable remove-replica, failover, and switchover.
  (Stories: P1 Opt In, P1 Safety)
- FR-020: Existing replica-agent and network-protocol tests MUST pass without
  changing their asserted commands, fencing rules, response classifications, or
  ordering solely to accommodate the durable workflow. (Stories: P1 Safety)

### Safety Parity Inventory

The durable path passes FR-004 only if tests and trace evidence establish all of
the following observable properties:

1. **Admission**: the target is a non-primary stable secondary with the exact
   admitted incarnation and Pod UID; retained membership satisfies minimum
   replica and previous-write-quorum requirements.
2. **Primary authority**: commands use the exact primary incarnation,
   generation, control version, runtime epoch, protocol version, and frozen
   configuration identity accepted for the operation.
3. **Mode behavior**: ScaleDown requires reachable lifecycle authority and exact
   target generation; Force may proceed without target authority but does not
   relax topology, quorum, primary, UID, epoch, or configuration fences; an
   accepted ScaleDown does not silently convert to Force.
4. **Primary command**: the operation issues the same coarse correlated removal
   intent as the explicit workflow, with an identity stable across replay.
5. **Observations**: progress is derived from authoritative primary identity,
   health, writable epoch/configuration, correlated current or retained action
   evidence, coordinator progress/result, exact target connection state, target
   lifecycle state, Pod UID, and role label.
6. **Pre-commit retry**: proven precondition or continuity rejection permits a
   fenced refresh; busy permits retry of the same intent; conflicts terminate
   as unsafe; unknown transport outcomes are resolved by authoritative
   observation before any redrive.
7. **Bounded redrive**: a changed primary-agent generation may redrive only from
   an exact safe pre-commit configuration and within the existing attempt bound.
8. **Irreversible boundary**: exact reduced Current commit evidence is durable
   before any target cleanup; once current-install dispatch may have occurred,
   ambiguity cannot authorize restoration of previous Current.
9. **Cleanup**: connection absence, terminal retirement evidence, exact-UID role
   mutation, and exact-UID deletion are established before stable topology
   publication.
10. **Terminal dispositions**: success, proven-safe compensated failure, and
    unsafe ambiguity map to outcomes equivalent to the explicit workflow's
    completed, failed, and typed poisoned dispositions.
11. **Publication ordering**: a compact terminal checkpoint is durably accepted
    and observable before final topology and operation status are published.
12. **Replay**: restart at every durable boundary reproduces the same accepted
    command, observation interpretation, and terminal disposition.

### Amortization Measurement and Classification

Measurements use executable lines and decision points as separate dimensions.
The tool MUST print every component before calculating totals.

- `legacy_remove`: the non-overlapping comparable scope of the explicit
  remove-replica workflow.
- `remove_body`: the kernel-hosted remove-replica orchestration body, excluding
  shared infrastructure and operator integration.
- `remove_integration`: remove-replica-specific admission, routing, status,
  persistence bridge, telemetry, and effect integration not reusable by
  switchover.
- `shared_before`: the shared reusable infrastructure charged by the existing
  switchover measurement at the base revision.
- `shared_after`: the shared reusable infrastructure after serving both
  workflows.
- `shared_growth = shared_after - shared_before`.
- `remove_marginal = remove_body + remove_integration + shared_growth`.
- `marginal_ratio = remove_marginal / legacy_remove`.
- `shared_growth_ratio = shared_growth / shared_before`.

The work item classifies amortization independently for executable lines and
decision points, then combines the two:

- **Positive**: `marginal_ratio < 1.0` in both dimensions and
  `shared_growth_ratio <= 0.25` in both dimensions.
- **Negative**: `marginal_ratio >= 1.0` in either dimension or
  `shared_growth_ratio > 0.50` in either dimension.
- **Inconclusive/mixed**: all other results, including a marginal reduction
  paired with shared growth between 25% and 50%.

Nested diagnostic scopes may be reported but MUST NOT be added to charged
totals. The tool MUST reject duplicate files, intersecting ranges, missing
markers, reversed ranges, and any overlap between charged scopes.

### Validation Coverage Matrix

The implementation is not complete until the following coverage passes:

- Default-explicit routing and unsupported durable-selection rejection.
- Successful durable ScaleDown removal and applicable Force-mode admission and
  fencing behavior.
- Typed input/result bounds and deterministic replay mismatch rejection.
- Exact prepared-command identity, exposure-before-dispatch, single-use permit,
  and preparation rejection on authority or identity drift.
- Restart before dispatch, after exposure, after observation, after commit,
  during cleanup, after terminal persistence, and before status publication.
- Proven-no-admission redelivery, lost reply, compare-and-swap conflict, unknown
  checkpoint outcome, and quarantined reload.
- Pre-commit compensation, post-dispatch ambiguity, bounded redrive, and typed
  terminal dispositions.
- Active checkpoint admission, result reservation, terminal compaction,
  terminal reload without workflow polling, and checkpoint ownership/budget
  enforcement.
- Shared effect-adapter outcome classification and all previously supported
  switchover command classes.
- Existing switchover pilot unit and reconciler matrices.
- Existing explicit add-replica, remove-replica, failover, and replica lifecycle
  regressions.
- Formatting, all-target compilation, all-feature warning-free linting,
  durable-execution default/Kubernetes suites, feasibility evidence, and
  complexity measurement.
- The known live-cluster `kuberic-tests::kvstore_k8s::test_kvstore_k8s_write_read`
  test may be reported as an environment-related exception only when its failure
  is attributable to stale local cluster deployment; no other validation
  failure is covered by this exception.

### Key Entities

- Durable remove-replica execution: An opt-in, replayable operation identified
  independently from the explicit status-machine operation.
- Prepared effect: An exact, fenced external command durably recorded before a
  single dispatch permit is issued.
- Authoritative observation: A replayable result derived from current cluster,
  agent, or persistence authority rather than process-local assumptions.
- Commit evidence: Durable proof that the reduced Current configuration crossed
  the irreversible removal boundary.
- Terminal outcome: A compact immutable completion, safe failure, or quarantine
  result persisted before final status publication.
- Charged scope: A uniquely attributed source range included in complexity
  accounting without overlap with another charged scope.

### Cross-Cutting / Non-Functional Requirements

- Measurements MUST be reproducible with repository tooling and identify the
  tested revision and enabled features.
- The implementation MUST remain deterministic under replay and bounded under
  admitted activity, transition, result, terminal, and encoded-size limits.
- Documentation MUST distinguish write attempts from accepted writes, external
  effects from passive observations, stable ceilings from run-specific sizes,
  and proven conclusions from hypotheses.
- The feature MUST remain experimental and default-off.

## Success Criteria

- SC-001: Default builds and non-opted-in sets continue to execute the explicit
  remove-replica workflow in existing regression tests. (FR-001, FR-002)
- SC-002: Feature-enabled durable tests execute a successful remove-replica
  operation through accepted checkpoint creation, prepared effect dispatch,
  authoritative observations, terminal persistence, and final publication.
  (FR-002, FR-004, FR-005, FR-009)
- SC-003: Restart, redelivery, lost reply, stale identity, persistence conflict,
  unknown outcome, post-commit ambiguity, and terminal reload tests demonstrate
  no duplicate unfenced effect and no unsafe inferred outcome. (FR-006, FR-007,
  FR-008)
- SC-004: All admitted representative executions remain below the stable encoded
  checkpoint ceiling and terminal payload ceiling, with actual byte sizes
  reported as run-specific ranges. (FR-010, FR-015)
- SC-005: Complexity measurement completes with no charged-scope overlap and
  prints the second workflow's body/comparable/marginal scopes plus shared
  infrastructure growth and combined totals. (FR-013, FR-014)
- SC-006: The documented amortization conclusion follows directly from the
  measured marginal and shared-scope results, including a negative or
  inconclusive conclusion if warranted. (FR-011, FR-013, FR-018)
- SC-007: Feasibility evidence passes a development-dependency case, rejects a
  library-runtime fixture, and prints the classifier's recomputed result.
  (FR-016, FR-017)
- SC-008: Formatting, all-target checks, all-feature clippy, durable-execution
  default and Kubernetes-feature suites, operator pilot/durable removal tests,
  and lifecycle regressions pass except for any explicitly identified
  environment-dependent live-cluster failure. (FR-012, FR-019, FR-020)
- SC-009: README and roadmap claims match feature routing, executable
  feasibility evidence, complexity output, and checkpoint measurements.
  (FR-017, FR-018)

## Assumptions

- Remove-replica remains the second kernel-hosted workflow; changing the port
  target requires explicit scope approval.
- The existing remove-replica protocol is the semantic baseline; this work does
  not redesign ReplicaAgent or gRPC behavior.
- Existing stable admission ceilings remain applicable unless measured
  remove-replica requirements prove that a reviewed, evidence-backed change is
  necessary.
- The existing explicit CRD-status workflow remains present for fallback,
  comparison, and default operation.
- Run-specific byte measurements can vary with generated identifiers and
  serialization while stable admission ceilings remain contractual.
- Existing repository tooling is sufficient for implementation and validation.

## Scope

In Scope:

- A dedicated default-off feature and explicit runtime selection for durable
  remove-replica.
- Kernel workflow, typed activities, prepared effects, observations, replay,
  terminal outcomes, checkpoint admission, and ConfigMap persistence.
- Deliberate generalization of existing switchover pilot infrastructure needed
  by exactly these two workflows.
- Unit and reconciler coverage for durable remove-replica and regressions for
  existing topology workflows.
- Complexity and representative checkpoint/write measurements.
- Feasibility predicate correction and README/roadmap updates.

Out of Scope:

- Removing or replacing the existing explicit remove-replica implementation.
- Enabling any durable workflow by default.
- Porting add-replica, failover, create-partition, or election workflows.
- Broad durable framework graduation or a claim that all future workflows will
  amortize similarly.
- ReplicaAgent semantic changes, gRPC protocol redesign, or compatibility
  guarantees for this unstable project.
- Modifying local cluster deployments to make an environment-stale live-cluster
  test pass.

## Dependencies

- Existing durable-execution workflow, typed activity, checkpoint, host, and
  Kubernetes provider contracts.
- Existing switchover pilot effect adapters, checkpoint-store integration, and
  reconciler bridge.
- Existing explicit remove-replica workflow and lifecycle tests as the semantic
  baseline.
- Cargo feature and CRD/runtime selection mechanisms.
- Repository complexity-measurement and validation tooling.

## Risks & Mitigations

- Shared infrastructure grows proportionally to the second workflow, disproving
  amortization. Mitigation: retain disjoint measurements and report the result
  plainly rather than reclassifying workflow-specific code as shared.
- Generalization regresses switchover. Mitigation: preserve and run the complete
  feature-enabled pilot matrix and shared-adapter tests.
- Durable and explicit remove semantics diverge. Mitigation: map every command,
  observation, fence, commit point, cleanup step, and terminal disposition to
  existing contract evidence and parity tests.
- Unknown outcomes cause duplicate or unsafe effects. Mitigation: require
  prepared exposure, single-use permits, authoritative reload, and quarantine
  for unresolvable ambiguity.
- Checkpoints exceed admitted limits. Mitigation: project worst-case bounds at
  admission and record measured active/terminal sizes in representative tests.
- Runtime-neutrality testing becomes a rubber stamp. Mitigation: parse or
  isolate the library dependency scope and add a negative fixture containing a
  real library runtime dependency.
- Documentation overstates evidence. Mitigation: distinguish stable contracts,
  measured snapshots, and conclusions in both generated output and prose.

## References

- User brief dated 2026-09-07.
- Research: `.paw/work/durable-remove-replica/SpecResearch.md`
- Existing scope: `durable-execution/README.md`
- Existing roadmap: `docs/features/kuberic/durable-execution-roadmap.md`
