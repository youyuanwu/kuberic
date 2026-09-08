# Feature Specification: Framework-Native Switchover Graduation

**Branch**: `feature/framework-native-switchover` | **Created**: 2026-09-08 | **Status**: Draft
**Input Brief**: Replace the explicit-default and feature-gated switchover split
with one production framework-native durable workflow.

## Overview

Kuberic operators need switchover to have one production execution model whose
progress survives operator restarts, ambiguous external effects, persistence
conflicts, and publication retries without duplicating commands or publishing
an unattested topology. The current product exposes two choices: an explicit
status-backed workflow and an optional durable pilot. Graduation removes that
choice and makes the durable framework path the only accepted and recovered
switchover behavior.

The graduated workflow must retain the existing safety envelope. It must revoke
writes before freezing progress, require the intended target to catch up,
preserve exact replica incarnation and agent-generation fences, order demotion,
promotion, epoch distribution, configuration changes, and routing publication,
and compensate only where the observed commit boundary still permits it.
Unknown effects and incompatible persisted state must stop progress safely
rather than trigger an unproven retry or a replacement execution.

The migration also resolves the documented ownership question for local
switchover mutations. Current research shows that individually correlated
local actions already provide the required fencing and recovery evidence,
while one agent-owned intent would require new coordination semantics between
the old primary, target, and retained replicas. This graduation therefore
retains the fine-grained correlated action boundary and makes the durable execution the
single global authority. A new coarse request that owns the full local
sequence, or a new cross-replica coordination surface, is not part of this
work.

## Objectives

- Provide one default and only production switchover execution behavior.
- Preserve deterministic, restart-safe, fail-closed switchover progress.
- Recover active and completed switchovers from bounded persistent state
  without adding another operator-facing runtime or service.
- Establish measurable switchover-specific capacity limits for active and
  completed execution state.
- Keep each local mutation independently correlated and observable so an
  uncertain result never authorizes an unproven retry.
- Replace old selection, duplicate observability, and obsolete documentation
  surfaces only after the enumerated safety scenarios have replacement
  coverage.
- Preserve behavior of remove-replica, add/build, failover, and initial
  creation.

## User Scenarios & Testing

### User Story P1.1 – Execute a fenced durable switchover

Narrative: As a cluster operator, I request a healthy secondary to become the
primary and expect the topology to change without admitting writes on an
unfenced or insufficiently caught-up replica.

Independent Test: Request a switchover on a healthy three-member set and verify
the requested target becomes the only published primary after the ordered
durable workflow completes.

Acceptance Scenarios:

1. Given a healthy set and an eligible target secondary, when switchover is
   requested, then writes are revoked, the target reaches the frozen progress
   point, role and epoch changes occur in the safe order, and the validated new
   topology is published.
2. Given a target that cannot reach the frozen progress point before its
   deadline, when the workflow evaluates the timeout, then it does not promote
   the target and instead follows the bounded safe recovery path.
3. Given any addressed replica with a changed incarnation, process generation,
   control version, or incompatible epoch, when progress is evaluated, then no
   stale command is dispatched and the workflow fails closed.

### User Story P1.2 – Recover without duplicating uncertain effects

Narrative: As an operator administrator, I expect switchover to recover after
process restarts and ambiguous persistence or RPC outcomes while preserving
the exact identity of each intended command.

Independent Test: Restart the operator after every durable boundary and inject
lost replies, unknown checkpoint outcomes, conflicts, and publication failures;
verify the execution completes or quarantines safely without unproven command
redelivery.

Acceptance Scenarios:

1. Given an active switchover checkpoint, when the operator restarts, then
   reconciliation reloads and validates the same execution before making
   further progress.
2. Given an exposed command with an unknown result, when reconciliation
   resumes, then authoritative observation resolves the exact command or the
   execution remains quarantined; the system does not infer non-execution.
3. Given a terminal checkpoint whose topology has not yet been published, when
   reconciliation retries, then it reloads and revalidates the terminal result
   and publishes exactly the validated topology.
4. Given a conflicting or unknown checkpoint write, when reconciliation
   retries, then it reloads authoritative state before preparing or exposing
   another effect.

### User Story P1.3 – Compensate failed switchovers safely

Narrative: As a cluster operator, I expect a failed switchover to restore a
provably stable topology where compensation is still permitted, without
rolling back an irreversible configuration boundary.

Independent Test: Inject failures before promotion, during uncertain
promotion, and after later configuration transitions; verify each path either
publishes an attested compensated topology or stops in a durable fail-closed
state.

Acceptance Scenarios:

1. Given failure before target promotion, when compensation begins, then the
   previous primary configuration is restored and validated before
   publication.
2. Given target promotion may have taken effect, when compensation proceeds,
   then the old primary is restored only through the new fenced epoch and all
   retained members and routing labels are converged before publication.
3. Given observations cannot prove either the intended or compensated stable
   topology, when terminal evaluation occurs, then no stable snapshot is
   published.

### User Story P2.1 – Upgrade conservatively from historical state

Narrative: As an administrator upgrading an unstable Kuberic deployment, I
expect historical explicit or pilot switchover state to be recognized and
blocked rather than silently resumed under a different contract.

Independent Test: Present every known legacy and malformed switchover status or
checkpoint shape and verify it becomes durable incompatibility evidence that
does not authorize a new execution.

Acceptance Scenarios:

1. Given an explicit version-1 execution or pilot version-1 or version-2
   execution, when the new controller reconciles it, then it records the source identity and
   incompatibility durably and does not clear it as absent.
2. Given an unsupported version, envelope, immutable contract, terminal shape,
   or malformed reference, when it is loaded, then progress stops with a
   stable blocking condition.
3. Given an accepted current execution whose checkpoint has not yet been
   created, when it is reconciled, then the admission ordering is
   distinguished from an unsupported or lost historical execution using
   durable identity evidence.

### User Story P2.2 – Operate a single production surface

Narrative: As a deployer or contributor, I want one switchover configuration,
installation, observability, and documentation path so production behavior is
not split by an optional pilot.

Independent Test: Build and test the workspace without a switchover execution
feature or selector and verify switchover always uses the framework-native
workflow.

Acceptance Scenarios:

1. Given a new switchover request, when it is accepted, then no public
   execution-mode choice is required or honored.
2. Given the ordinary installation and examples, when they are used, then
   framework-native switchover support is present without an optional
   capability switch.
3. Given status and conditions are observed, when switchover is active or
   blocked, then exactly one production execution surface identifies its
   admission, checkpoint, compatibility, and terminal state.

### User Story P2.3 – Enforce measured switchover-specific limits

Narrative: As a maintainer, I need the workflow to prove that its largest
declared input, result, active history, and terminal outcome fit bounded
storage before external effects are admitted.

Independent Test: Run canonical success and maximum-fault projections plus
exact one-byte-over tests for every independent contract dimension.

Acceptance Scenarios:

1. Given the canonical no-fault workflow, when measurements run, then external
   effects, passive observations, durable boundaries, accepted writes, active
   bytes, terminal bytes, and terminal-payload bytes are reported separately.
2. Given every supported compensation and redelivery history, when projected,
   then it fits the admitted activity, transition, input, result, active,
   terminal, payload, error, and host-fuel bounds.
3. Given any declared dimension exceeds its independent bound by one unit,
   when admission or progression validates it, then it is rejected before an
   unsafe external effect or unpersistable terminal result.

### Edge Cases

- The requested target is already primary, absent from the admitted stable
  snapshot, or no longer has the admitted UID.
- Epoch arithmetic overflows or a replica reports an epoch outside the
  admitted previous/target topology.
- The old primary is unavailable before the frozen progress point is captured.
- The target is available but cannot prove catch-up.
- A process generation changes after command preparation or dispatch.
- A command reply is lost after the effect may have been admitted.
- A routing-label write has an unknown outcome.
- Checkpoint create or compare-and-swap returns conflict or unknown outcome.
- A terminal checkpoint exists but status publication fails repeatedly.
- A historical explicit operation, pilot v1/v2 reference, malformed
  reference, unsupported envelope, or incompatible immutable contract is
  present.
- Any proposal to replace individually correlated local mutations would
  require new cross-replica coordination semantics and is outside this
  graduation.

## Requirements

### Functional Requirements

- FR-001: Switchover SHALL have one default and only production execution path
  backed by durable execution state. (Stories: P1.1, P2.2)
- FR-002: New admission SHALL reject an already-primary or non-member target
  and bind immutable execution identity to the admitted set, snapshot,
  operation version,   target, and exact replica incarnations. (Stories: P1.1)
- FR-003: Progress SHALL revoke old-primary writes before capturing a frozen
  progress point and SHALL require the exact target incarnation to reach that
  point before promotion. (Stories: P1.1)
- FR-004: The observable order SHALL be: revoke old-primary writes; capture
  frozen progress; prove target catch-up; demote the old primary; promote the
  target; distribute the new epoch to retained replicas; establish catch-up
  configuration; prove write quorum; establish current configuration; update
  exact-incarnation routing; attest all members; publish the stable topology.
  No later step SHALL occur before the preceding proof exists. (Stories: P1.1)
- FR-005: Every external command SHALL have stable logical identity, exact
  encoded content, addressed replica identity, UID/generation/control-version
  fences where applicable, and an authoritative observation rule for
  recovery. (Stories: P1.2)
- FR-006: Unknown effects SHALL be quarantined until authoritative observation
  proves the exact effect or proves bounded non-admission; otherwise the
  execution SHALL remain durably blocked without another dispatch.
  (Stories: P1.2)
- FR-007: Reconciliation SHALL remain the sole scheduler; no worker, queue,
  lease, watcher, separate service, or independent retry scheduler SHALL be
  required. (Stories: P1.2, P2.2)
- FR-008: The workflow SHALL reload authoritative state after conflict,
  unknown persistence outcome, or process restart before preparing another
  effect. (Stories: P1.2)
- FR-009: Terminal success or compensation SHALL be persisted and revalidated
  independently of topology publication, and later reconciles SHALL reload the
  terminal result without replaying discarded active history. (Stories: P1.2,
  P1.3)
- FR-010: Compensation before target promotion SHALL restore and attest the
  previous epoch and primary topology. Compensation after target promotion may
  proceed only through the new epoch and SHALL attest the old primary restored
  as primary, every retained replica at the compensating epoch and role, the
  required configuration state, and exact-incarnation routing before
  publication. If either topology cannot be attested, publication SHALL remain
  blocked. (Stories: P1.3)
- FR-011: Each local switchover mutation SHALL retain independent command
  identity and an independent authoritative observation rule under the global
  durable execution. No single local request SHALL encapsulate the entire
  cross-replica sequence. (Stories: P1.1, P1.2, P1.3)
- FR-012: The production switchover contract SHALL be compact, independently
  versioned, and operation-specific; each limit SHALL be supported by
  switchover measurement evidence rather than another operation's constants.
  (Stories: P2.3)
- FR-013: Independent limits SHALL cover activity count, workflow transitions,
  decoded workflow input, decoded activity input, decoded result, active
  encoded checkpoint, terminal encoded checkpoint, terminal payload, bounded
  error text, replica count, and per-reconcile progress fuel. (Stories: P2.3)
- FR-014: The canonical three-member success history SHALL preserve the
  established baseline of 9 external effects, 3 passive observations, 12
  completed durable boundaries, and 13 accepted writes. Capacity projection
  SHALL cover at least the 19-record success-with-redelivery history and the
  21-record rollback-with-redelivery history, and exact one-unit-over coverage
  SHALL reject every independent bound. (Stories: P2.3)
- FR-015: Known legacy explicit and pilot execution state SHALL be converted
  or represented as durable incompatibility evidence preserving source
  identity, version, checkpoint reference where present, and fingerprint;
  incompatible state SHALL not be resumed, cleared as absent, or replaced by
  a new execution. Recognized legacy sources SHALL include explicit operation
  version 1 and pilot execution versions 1 and 2. (Stories: P2.1)
- FR-016: Unsupported checkpoint envelope, immutable execution contract,
  activity version, terminal shape, status shape, or malformed reference SHALL
  produce a stable blocking disposition and condition. (Stories: P2.1)
- FR-017: The public switchover execution-mode selector, optional pilot
  capability, split acceptance and recovery behavior, and duplicate status and
  condition surfaces SHALL be removed only after replacement coverage passes.
  (Stories: P2.2)
- FR-018: The ordinary installation, public schema, and examples SHALL include
  the sole switchover path without opt-in configuration. (Stories: P2.2)
- FR-019: User and maintainer documentation SHALL describe the graduated ownership boundary,
  contract/version, failure and compatibility semantics, operation-specific
  measurements, and unchanged scope of other topology operations. (Stories:
  P2.2, P2.3)
- FR-020: Framework-native remove-replica behavior and existing add/build,
  failover, and initial-creation behavior SHALL continue to pass their existing
  success, failure, restart, and publication suites. (Stories: P2.2)

### Key Entities

- **Switchover Admission**: Immutable semantic identity of one requested
  topology transition and its admitted replica incarnations.
- **Switchover Execution Contract**: Versioned compact execution input,
  activity/result schemas, limits, and terminal outcome.
- **Prepared Command**: Exact correlated external effect with the authority and
  observation evidence required for safe dispatch and recovery.
- **Checkpoint Reference**: Status-owned identity of the persistent record
  containing active or terminal execution state.
- **Incompatibility Evidence**: Durable record that a historical or malformed
  execution cannot be resumed under the production contract.
- **Terminal Outcome**: Immutable success, compensated, or fail-closed result
  that must be revalidated before stable topology publication.

### Cross-Cutting / Non-Functional Requirements

- NFR-001: No worker, queue, lease, watcher, separate durable service, generic
  orchestration platform, or independent retry scheduler may be introduced.
- NFR-002: Checkpoint growth and all retained local status must be bounded by
  immutable admitted limits.
- NFR-003: Storage or RPC ambiguity must prefer delayed progress over duplicate
  or unauthoritative mutation.
- NFR-004: Breaking removal of unstable public API fields is allowed, but
  persisted historical execution must remain conservatively recognized.
- NFR-005: Every existing acceptance and regression suite affected by the
  change must complete with zero failures.

## Success Criteria

- SC-001: A stable three-member topology with one current primary and two
  active secondaries reaches one validated new primary
  through the sole production execution path with no public selector or
  optional switchover feature. (FR-001, FR-003, FR-004, FR-017, FR-018)
- SC-002: Restart-after-every-boundary, lost-reply, conflict, unknown-write,
  unknown-effect, stale UID, stale generation, publication-retry, and terminal
  reload tests complete safely without an unproven duplicate effect.
  (FR-005, FR-006, FR-008, FR-009)
- SC-003: Failure injection before and around promotion produces only an
  exactly attested compensated topology or a durable fail-closed result.
  (FR-010)
- SC-004: Every known legacy or malformed switchover state fixture is blocked
  durably and never authorizes a replacement execution. (FR-015, FR-016)
- SC-005: The graduated path uses no single local request for the full
  cross-replica sequence, and tests prove exact command identity, duplicate
  replay, generation change, progress, compensation, and terminal
  interpretation for the retained individually correlated mutations.
  (FR-011)
- SC-006: The canonical no-fault measurement reports external effects, passive
  observations, durable boundaries, accepted writes, active bytes, terminal
  bytes, and terminal-payload bytes as separate values. (FR-014)
- SC-007: The 19-record success-with-redelivery and 21-record
  rollback-with-redelivery projections fit admitted switchover-specific
  bounds, while exact one-unit-over tests fail independently for every bounded
  dimension. (FR-012, FR-013, FR-014)
- SC-008: Every affected quality, measurement, persistence, targeted
  switchover, and full regression gate completes with zero failures. (FR-018,
  FR-020, NFR-005)
- SC-009: Documentation and examples contain no instructions for selecting an
  explicit or pilot switchover path and accurately describe the graduated
  contract, measurements, recovery, and compatibility behavior. (FR-019)
- SC-010: Existing remove-replica, add/build, failover, and initial-creation
  success, failure, restart, and publication test suites complete with zero
  regressions. (FR-020)

## Assumptions

- The repository remains unstable, so removal of the public selector and
  duplicate status fields is acceptable.
- Current clusters may retain explicit operation v1 or pilot v1/v2 state even
  though their prevalence cannot be measured from the repository.
- A missing persistent record is safe only when current admission evidence
  proves the reference was persisted before first record creation; other missing or
  incompatible historical state is not absence.
- Persistent execution storage remains same-namespace and owner-bound, with
  cleanup rules inherited from the shared provider contract.
- Environment-dependent validation runs when the repository's established
  test environment is available; deterministic persistence and high-fidelity
  reconciliation tests remain mandatory.

## Scope

### In Scope

- Framework-native switchover admission, execution, recovery, compensation,
  terminal handling, and topology publication.
- Retention of individually correlated local switchover mutations under one
  durable global execution.
- Switchover-specific compact contract, versioning, bounds, projections, and
  measurements.
- Legacy explicit/pilot compatibility recognition and fail-closed state.
- Removal of switchover selector, pilot feature, duplicate routing/status/
  resource surfaces, and superseded tests and documentation.
- Public schema, installation manifests, examples, measurements, operational
  guidance, protocol guidance, testing guidance, and derived artifacts
  required by the change.

### Out of Scope

- Migrating add-replica, failover, or initial partition creation to the
  framework.
- Changing graduated framework-native remove-replica semantics.
- Addressing SQLite issue #42.
- Introducing a general redesign of local replica control or network
  interfaces beyond demonstrable switchover requirements.
- Adding workers, queues, leases, watchers, another scheduler, a separate
  durable service, or a generic orchestration platform.
- Mixed-version checkpoint migration that attempts to resume historical
  contracts under the new workflow.

## Dependencies

- Existing bounded durable execution and persistence capabilities.
- Existing independently correlated local replica commands and
  exact-incarnation routing updates.
- Stable topology observation and publication behavior.
- Existing acceptance, recovery, persistence, and regression test
  capabilities.

## Risks & Mitigations

- **Unsafe ownership transfer**: A coarse intent could require unsupported
  cross-agent coordination or obscure the global durable authority.
  **Mitigation**: Require code-research evidence and retain the existing
  fine-grained boundary if it is safer within scope.
- **Legacy state mistaken for absence**: An upgrade could start a second
  execution over unresolved effects. **Mitigation**: Persist incompatibility
  evidence and block replacement admission.
- **Bound copied from another operation**: Valid switchover histories could
  exceed storage or terminal capacity. **Mitigation**: Measure independent
  success and maximum-fault projections and add exact-bound rejection tests.
- **Unknown-effect redelivery**: Lost replies could duplicate a role or
  configuration mutation. **Mitigation**: Preserve exact command identity,
  one-use dispatch, quarantine, and observation-first recovery.
- **Premature publication**: A durable terminal could be published against
  changed pods or topology. **Mitigation**: Revalidate exact UID, generation,
  epoch, role, configuration, routing, identity, and terminal accounting at
  every publication retry.
- **Regression in adjacent operations**: Shared execution or public-schema
  changes could alter remove/add/failover/create behavior. **Mitigation**:
  isolate switchover behavior and run targeted plus full regression suites.

## References

- Specification research for Framework-Native Switchover Graduation,
  2026-09-08.
- Prior framework-native remove-replica graduation evidence.
- Current durable execution roadmap and Kuberic status documentation.
