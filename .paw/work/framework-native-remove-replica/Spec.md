# Feature Specification: Framework-Native Remove Replica

**Branch**: `feature/framework-native-remove-replica` | **Created**: 2026-09-07 | **Status**: Approved for Planning
**Input Brief**: Graduate remove-replica from an explicit-versus-durable pilot into the default and only framework-native production workflow.

## Overview

Kuberic operators need remove-replica operations to recover deterministically across controller restarts, ambiguous external outcomes, and checkpoint conflicts without choosing between two orchestration engines. The product direction is now committed to the durable framework, so remove-replica must operate through one production path with one persisted execution contract and one set of safety semantics.

The migration must preserve the exact authority, fencing, cleanup, and publication guarantees already exercised by the explicit and pilot implementations while removing pilot-only configuration, compatibility projections, telemetry, and documentation. Old experimental checkpoints may be rejected rather than migrated because the prior path was default-off and the project permits breaking changes, but they must never be silently interpreted as the new production contract.

The new workflow must use compact immutable commands, evidence, identifiers, and state deltas rather than copying complete mutable state and configuration descriptors into every durable activity boundary. A shared production runner must centralize reusable checkpoint, recovery, permit, quarantine, terminal, and bounded-progression behavior so the next framework-native workflow can reuse it without introducing speculative workers, queues, or distributed scheduling.

## Objectives

- Provide a single default remove-replica orchestration path with deterministic durable recovery.
- Preserve all existing safety and lifecycle behavior before deleting the explicit implementation.
- Centralize reusable durable-runner mechanics while keeping workflow-specific authority and effects explicit.
- Reduce the representative active checkpoint maximum from the measured 93,841-byte baseline to at most 49,152 bytes.
- Preserve the representative five durable boundaries and six accepted checkpoint writes unless a separately evidenced design safely reduces them.
- Remove comparison-pilot surfaces and document remove-replica as a production migration.
- Define stable extension points that can support a future add-replica migration without porting add-replica in this work item.

## User Scenarios & Testing

### User Story P1 – Remove a replica through one production workflow

Narrative: As a Kuberic operator, I request removal of a non-primary replica and receive the same safe lifecycle behavior without selecting an orchestration mode.

Independent Test: Submit an eligible remove request to a healthy three-member set and verify that it completes through the durable workflow, publishes the reduced topology only after durable terminal state, and exposes no mode selector.

Acceptance Scenarios:

1. Given an eligible ScaleDown request, when removal succeeds, then the exact target is retired, disconnected, UID-fenced, deleted, durably terminal, and removed from published topology in that order.
2. Given an eligible Force request, when removal crosses the irreversible commit boundary, then retries and restarts never compensate back to the previous topology.
3. Given an ineligible request, when admission evaluates current authority, then the operation is rejected with an explicit typed domain reason and no unsafe effect is exposed.

### User Story P1 – Recover conservatively from interruption and ambiguity

Narrative: As an operator, I expect controller restarts, lost replies, and uncertain checkpoint writes to recover from durable evidence without duplicating an uncertain external effect.

Independent Test: Inject restart, lost-reply, conflict, and unknown-write outcomes at every durable boundary and verify deterministic recovery and exact effect counts.

Acceptance Scenarios:

1. Given a restart before commit, when reconciliation resumes, then the workflow either continues safely or compensates according to authoritative evidence.
2. Given a restart after commit, when reconciliation resumes, then cleanup completes and the committed topology is never rolled back.
3. Given an exposed effect with a lost reply or uncertain checkpoint outcome, when the workflow reloads, then it quarantines and resolves the exposed activity from authoritative observation before issuing another permit.
4. Given a checkpoint conflict, when the authoritative checkpoint is reloaded, then progression resumes from the stored state rather than replaying stale local intent.

### User Story P1 – Preserve authority and fencing guarantees

Narrative: As a cluster owner, I need removal to act only on the intended replica, epoch, incarnation, configuration, and Kubernetes object.

Independent Test: Mutate each authority input independently and verify that the workflow waits, rejects, or quarantines conservatively without status churn or unintended effects.

Acceptance Scenarios:

1. Given temporary loss of exact primary or target status, when reconciliation runs, then it waits without changing operation generation or publishing an invalid terminal result.
2. Given epoch, incarnation, UID, control-version, configuration, or command-signature drift, when a command or observation is validated, then stale authority is rejected.
3. Given cleanup for a replaced Kubernetes object with the same name, when label removal or deletion executes, then UID fencing prevents mutation of the replacement.
4. Given post-commit cleanup, when completion is considered, then exact connection absence and retirement evidence are required.

### User Story P1 – Persist compact bounded workflow history

Narrative: As a platform operator, I need durable persistence to remain bounded and operationally inspectable without repeated full-state payloads dominating every checkpoint.

Independent Test: Run the representative three-sample no-fault three-member ScaleDown measurement fixture and verify semantic counts, checkpoint limits, and payload classifications.

Acceptance Scenarios:

1. Given the representative no-fault workflow, when five durable boundaries complete, then exactly six accepted checkpoint writes occur unless an evidence-backed change intentionally lowers the count.
2. Given any representative sample, when active checkpoints are measured using the canonical encoded envelope size, then the per-run maximum is no greater than 49,152 bytes.
3. Given terminalization, when the checkpoint is reloaded, then the active history is compacted to a bounded terminal record whose checkpoint and payload sizes are reported separately.
4. Given theoretical retry and fault histories, when admission is calculated, then record count, activity input/result bounds, encoded checkpoint capacity, and terminal payload capacity remain explicitly enforced.

### User Story P2 – Operate and extend a shared durable runner

Narrative: As a Kuberic maintainer, I need common durable-host mechanics implemented once while workflow-specific decisions, evidence, effects, and publication remain reviewable.

Independent Test: Exercise switchover and remove-replica through the shared runner and verify their workflow-specific safety tests still pass without per-workflow copies of the common control loop.

Acceptance Scenarios:

1. Given active, terminal, rejected, quarantined, conflicted, or unknown checkpoint outcomes, when either supported workflow reconciles, then shared runner behavior handles the common lifecycle consistently.
2. Given a workflow-specific external effect, when a prepared command is exposed, then the runner enforces one-use permits and delegates exact command validation and dispatch to the workflow adapter.
3. Given the future add-replica migration, when maintainers inspect the extension contract, then observation collection, effect preparation, deadline policy, terminal validation, and publication are explicit extension points without an added queue or worker architecture.

### User Story P2 – Remove pilot-era product surfaces

Narrative: As an operator or contributor, I need configuration, examples, telemetry, and documentation to describe the production behavior rather than a closed comparison experiment.

Independent Test: Search public and deployment surfaces for remove-replica mode selection and pilot terminology and verify only historical measurement context remains where necessary.

Acceptance Scenarios:

1. Given a current or newly generated custom resource, when remove-replica is requested, then no explicit/durable selector is available or required.
2. Given an old pilot checkpoint, when the new runner loads it, then it is rejected or quarantined as incompatible and is never treated as a valid native checkpoint or fresh execution.
3. Given updated documentation, when readers review remove-replica, then it is described as a production framework-native migration with current measurements and stable contracts.

### Edge Cases

- A checkpoint is malformed, exceeds bounds, uses an unsupported envelope or workflow contract version, or belongs to another execution.
- A terminal checkpoint exists while status or topology publication has not yet completed.
- Primary status temporarily omits exact configuration authority even though the operation may later become valid again.
- An effect was exposed but its reply, checkpoint write, or both are lost.
- A CAS conflict reveals a more advanced active or terminal checkpoint.
- The target name is reused with another Kubernetes UID during label cleanup or deletion.
- A post-commit target remains connected or has incomplete retirement evidence.
- A retained checkpoint outlives its owner or is processed by a cleanup identity.
- The current domain exceeds the supported maximum three-member assumption.
- A passive observation cannot safely be fused because fresh authority or fence data is required before the next command.

## Requirements

### Functional Requirements

- **FR-001**: Remove-replica shall use one framework-native durable orchestration path by default and shall expose no operator-selectable explicit/durable execution mode. (Stories: P1 single workflow, P2 pilot removal)
- **FR-002**: The new workflow contract shall have an explicit version/discriminator and shall reject or quarantine prior experimental checkpoints without silently converting them or treating them as absent. (Stories: P1 recovery, P2 pilot removal)
- **FR-003**: Admission shall validate exact primary and target identity, target non-primary membership, operation mode, quorum/minimum constraints, generation, control version, protocol, epoch, incarnation, UID, and configuration authority. (Stories: P1 single workflow, P1 authority)
- **FR-004**: Temporary absence of exact primary or target status shall produce a wait outcome without operation-generation or status churn. (Stories: P1 authority)
- **FR-005**: External commands shall be immutable, exactly matched, signed or equivalently bound to their operation/action/attempt authority, and exposed only after durable preparation. (Stories: P1 recovery, P1 authority)
- **FR-006**: Lost replies, unknown checkpoint-write outcomes, conflicts, and restarts shall recover from authoritative checkpoint and external observation without duplicating an uncertain exposed effect. (Stories: P1 recovery)
- **FR-007**: Pre-commit recovery shall permit only safe continuation or compensation; post-commit recovery shall never roll back the committed reduced topology. (Stories: P1 recovery)
- **FR-008**: Commit evidence shall be explicit, typed, monotonic, and validated against authoritative configuration observations before cleanup or terminal success. (Stories: P1 single workflow, P1 authority)
- **FR-009**: Post-commit completion shall require exact connection cleanup, retirement evidence, UID-fenced label cleanup, and UID-fenced deletion as applicable. (Stories: P1 single workflow, P1 authority)
- **FR-010**: Terminal state shall be durably stored and successfully reloaded before status or topology publication. (Stories: P1 single workflow, P1 compact persistence)
- **FR-011**: Domain rejection, incompatibility, unsafe disposition, compensation, and completion shall remain distinct typed outcomes. (Stories: P1 single workflow, P1 recovery)
- **FR-012**: The workflow shall preserve deterministic replay, bounded redelivery, bounded admission, and conservative quarantine of exposed activities. (Stories: P1 recovery, P1 compact persistence)
- **FR-013**: Active activity inputs and results shall carry compact commands, identifiers, evidence, and state deltas; they shall not repeat complete mutable workflow state or complete configuration descriptors unless a documented safety proof requires the specific field. (Stories: P1 compact persistence)
- **FR-014**: The representative no-fault three-member ScaleDown path shall retain five semantic durable boundaries—three external effects and two passive observations—and no more than six accepted checkpoint writes unless an evidenced design safely reduces either count. (Stories: P1 compact persistence)
- **FR-015**: Every representative active checkpoint sample shall remain at or below 49,152 encoded bytes, measured from the canonical checkpoint envelope. (Stories: P1 compact persistence)
- **FR-016**: Separate enforced bounds shall cover activity record count, activity input size, activity result size, total encoded checkpoint size, and terminal payload size under fault and retry histories. (Stories: P1 compact persistence)
- **FR-017**: A shared production durable runner shall centralize checkpoint lifecycle interpretation, fused progression, conflict and unknown-outcome reload, quarantine, permit lifecycle, terminal reload/handoff, and bounded requeue behavior. (Stories: P2 shared runner)
- **FR-018**: Fusion shall be the normal progression path; a workflow shall retain an unfused boundary only when fresh external observation or fence refresh is required before the next command. (Stories: P2 shared runner)
- **FR-019**: The shared runner shall leave observation gathering, exact effect preparation and dispatch, authority validation, deadline policy, terminal validation, and final publication workflow-specific. (Stories: P2 shared runner)
- **FR-020**: The migration shall remove the old explicit remove state machine only after traceability demonstrates equivalent or stronger native tests for every required safety invariant. (Stories: P1 single workflow, P1 recovery, P1 authority)
- **FR-021**: The migration shall remove remove-specific mode selectors, feature-gated runtime selection, parity projections, experiment-only telemetry, pilot-only conditions/status, and stale deployment schema. (Stories: P2 pilot removal)
- **FR-022**: General measurement tooling may remain, but remove-specific amortization classifications and thresholds that exist only for the closed comparison hypothesis shall be retired. (Stories: P2 pilot removal)
- **FR-023**: Checkpoint ownership, namespace validation, owner identity, garbage-collection behavior, and separately authorized retained-checkpoint cleanup shall remain enforced. (Stories: P1 compact persistence)
- **FR-024**: The migration shall not change ReplicaAgent or gRPC semantics unless implementation proves a protocol defect and obtains an explicit material scope decision. (Stories: P1 authority)
- **FR-025**: Add-replica shall not be ported, but documentation shall identify the stable runner extension points it can use in a later migration. (Stories: P2 shared runner)

### Key Entities

- **Native Remove Execution**: Immutable execution identity, contract version, admitted authority, and compact workflow state for one remove request.
- **Compact Activity**: A bounded durable boundary containing the minimum command, observation, evidence, identifiers, or delta required for deterministic progression.
- **Prepared Command**: An exact immutable external effect durably exposed before dispatch and consumed through a one-use permit.
- **Commit Evidence**: Monotonic proof that the reduced topology crossed the irreversible authority boundary.
- **Cleanup Proof**: Exact evidence that connection, retirement, Kubernetes labels, and deletion satisfy post-commit requirements.
- **Terminal Outcome**: Compact typed completion, compensation, rejection, incompatibility, or unsafe result that is persisted before publication.
- **Durable Runner Adapter**: Workflow-specific hooks for observation, effects, validation, deadlines, and publication used by the shared runner.

### Cross-Cutting / Non-Functional

- The workflow shall support the current maximum three-member remove domain unless a separately evidenced change intentionally generalizes it.
- Checkpoint and activity serialization shall be deterministic and reject unknown or incompatible contract shapes.
- The shared runner shall be a reviewable maintainability abstraction rather than macro-generated or lexically hidden workflow complexity.
- Reconciliation shall remain bounded per invocation and return a bounded deadline-aware requeue when work remains.
- Public documentation shall distinguish external effects from passive observations, durable boundaries from accepted writes, and active checkpoint, terminal checkpoint, and terminal payload measurements.

## Success Criteria

- **SC-001**: All remove requests execute through one production durable path, and no CRD, runtime, deployment, or example surface exposes a remove execution-mode selector. (FR-001, FR-021)
- **SC-002**: The complete required safety inventory has test traceability to passing framework-native unit, reconciler, lifecycle, and real-provider tests before the explicit implementation is deleted. (FR-003–FR-012, FR-020, FR-023)
- **SC-003**: Restart tests pass at every pre-commit and post-commit durable boundary with no unsafe rollback or duplicated uncertain effect. (FR-006, FR-007, FR-012)
- **SC-004**: Temporary exact-status gaps produce wait behavior with no status-generation churn. (FR-004)
- **SC-005**: The representative no-fault fixture reports three external effects, two passive observations, five durable boundaries, and no more than six accepted checkpoint writes per run. (FR-014)
- **SC-006**: Across at least three representative no-fault samples, every per-run active checkpoint maximum is at most 49,152 bytes, a minimum 47.6% improvement over the measured 93,841-byte baseline. (FR-013, FR-015)
- **SC-007**: Tests enforce active admission, terminal checkpoint, terminal payload, activity input/result, and record-count limits independently. (FR-016)
- **SC-008**: Old pilot checkpoints and malformed, corrupt, oversized, or wrong-execution checkpoints are explicitly rejected or quarantined and never silently restarted as native executions. (FR-002, FR-012)
- **SC-009**: Switchover and native remove-replica pass through one shared runner control loop while retaining workflow-specific authority and effect adapters. (FR-017–FR-019)
- **SC-010**: Full default and all-feature workspace checks, clippy, formatting, durable-execution suites, operator reconciler suites, affected remove/add/failover lifecycle regressions, retained measurement tests, isolated Kind provider tests when available, and both live-cluster KV tests pass. (FR-020, FR-023)
- **SC-011**: Updated documentation reports representative boundaries, accepted writes, per-run active maxima, terminal checkpoint size, and terminal payload size with correct lifecycle-versus-run-variation labels. (FR-022)
- **SC-012**: No ReplicaAgent or gRPC semantic change is present unless separately approved following evidence of a protocol defect. (FR-024)
- **SC-013**: Documentation identifies reusable observation, effect, deadline, terminal-validation, and publication extension points for add-replica without implementing add-replica or a speculative worker architecture. (FR-025)

## Assumptions

- The prior durable remove path was default-off and experimental, so rejecting its checkpoints is acceptable if incompatibility is explicit and documented.
- The current remove domain remains bounded to three-member configurations.
- The compact 49,152-byte gate is based on the measured 93,841-byte maximum and a conservative 43,509-byte compact-contract projection with approximately 13% headroom.
- Five semantic boundaries remain necessary because fresh observation separates authority-sensitive external effects; accepted writes include the terminal write.
- Existing ReplicaAgent and gRPC contracts provide sufficient exact intent, observation, and cleanup evidence.
- Local planning and implementation commits occur directly on the target feature branch; only one final PR is created to `main`.

## Scope

### In Scope

- Shared in-process operator durable runner.
- Framework-native remove workflow and compact activity/state/terminal contracts.
- Remove checkpoint versioning, incompatibility handling, admission, measurements, and owner/retention behavior.
- Safety traceability and migration of explicit regression coverage.
- Removal of explicit/durable selection and pilot-only production surfaces.
- Switchover adaptation to the shared runner where required to prove reuse.
- Directly affected CRD, deployment, CI, examples, tests, README, roadmap, testing, protocol, and operator documentation.
- Stable extension-point documentation for future add-replica migration.

### Out of Scope

- Porting add-replica to the durable framework.
- New generic workers, queues, leases, watchers, distributed ownership, or retry schedulers.
- Active-history compaction or Continue-as-New introduced solely to conceal oversized authoring contracts.
- Changes to ReplicaAgent or gRPC semantics without a separately approved protocol-defect decision.
- Generalizing remove beyond the current three-member domain without evidence and explicit scope expansion.
- Optimizing implementation structure solely to improve lexical source-complexity metrics.

## Dependencies

- Durable checkpoint envelope, typed activity, prepared resolver, fused host progression, and Kubernetes checkpoint provider.
- Existing exact remove intent, action signature, lifecycle observation, connection, retirement, and Kubernetes UID evidence.
- Operator reconciliation and status/topology publication.
- Isolated Kuberic Kind cluster and repository image workflow for live validation.

## Risks & Mitigations

- **Compatibility ambiguity**: Old pilot checkpoints could resemble native state. **Mitigation**: use a new explicit contract discriminator/version and test fail-closed incompatibility.
- **Stale authority under fusion**: Progression could schedule a command from outdated observations. **Mitigation**: make fusion normal only for proven-safe transitions and require fresh observation/fence boundaries before authority-sensitive commands.
- **Over-generalized runner**: Shared code could obscure workflow safety or encode remove-specific assumptions. **Mitigation**: centralize only host lifecycle mechanics and keep evidence, effects, deadlines, validation, and publication in typed adapters.
- **Payload regression**: Compact types could gradually reacquire complete state copies. **Mitigation**: enforce the 48 KiB representative gate plus per-activity and total admission bounds.
- **Coverage loss during deletion**: Explicit tests could disappear without equivalent native assertions. **Mitigation**: maintain a traceability matrix and delete explicit code only after mapped tests pass.
- **Publication race**: Status could report completion before terminal durability. **Mitigation**: require terminal write and terminal reload before publication.
- **Cleanup of replacement objects**: Name reuse could mutate or delete the wrong pod. **Mitigation**: preserve exact UID fencing for labels and deletion.
- **Shared-path regression**: Switchover or future workflows could inherit incorrect strict validation. **Mitigation**: keep workflow-specific authority validation in adapters and run switchover/add/failover regression suites.

## References

- Research: `.paw/work/framework-native-remove-replica/SpecResearch.md`
- Compact checkpoint budget analysis: PAW single-model research result dated 2026-09-07
- Product direction and safety inventory: user brief dated 2026-09-07
