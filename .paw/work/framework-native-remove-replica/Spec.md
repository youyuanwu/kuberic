# Feature Specification: Framework-Native Remove Replica

**Branch**: `feature/framework-native-remove-replica` | **Created**: 2026-09-07 | **Status**: Approved for Planning
**Input Brief**: Graduate remove-replica from an explicit-versus-durable pilot into the default and only framework-native production workflow.

## Overview

Kuberic operators need remove-replica operations to recover deterministically across controller restarts, ambiguous external outcomes, and checkpoint conflicts without choosing between two orchestration engines. The product direction is now committed to the durable framework, so remove-replica must operate through one production path with one persisted execution contract and one set of safety semantics.

The migration must preserve the exact authority, fencing, cleanup, and publication guarantees already exercised by the explicit and pilot implementations while removing pilot-only configuration, compatibility projections, telemetry, and documentation. Old experimental checkpoints may be rejected rather than migrated because the prior path was default-off and the project permits breaking changes, but they must never be silently interpreted as the new production contract.

The production workflow must keep durable records compact, bounded, and understandable rather than allowing repeated copies of complete operation state to dominate persistence. Common recovery behavior must be consistent across durable operator workflows, and the migration must leave a documented, testable reuse pattern for the next operation without adding new operational services.

## Objectives

- Provide a single default remove-replica orchestration path with deterministic durable recovery.
- Preserve all existing safety and lifecycle behavior before deleting the explicit implementation.
- Make common durable recovery outcomes consistent across supported operator workflows while preserving operation-specific authority and safety decisions.
- Reduce the representative active checkpoint maximum from the measured 93,841-byte baseline to at most 49,152 bytes.
- Preserve the representative five durable boundaries and six accepted checkpoint writes unless a separately evidenced design safely reduces them.
- Remove comparison-pilot surfaces and document remove-replica as a production migration.
- Demonstrate reuse by supporting both current durable operator workflows through one common recovery behavior and documenting the operation-specific responsibilities required by a future add-replica migration.

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

### User Story P2 – Maintain consistent reusable durable recovery

Narrative: As a Kuberic maintainer, I need supported durable workflows to recover consistently without duplicated control flow or hidden operation-specific safety rules.

Independent Test: Exercise switchover and remove-replica through the common recovery path and verify that both retain their operation-specific safety tests with no duplicated lifecycle control loop.

Acceptance Scenarios:

1. Given active, terminal, rejected, quarantined, conflicted, or unknown checkpoint outcomes, when either supported workflow resumes, then it follows the same tested lifecycle outcomes.
2. Given an operation-specific external effect, when it is durably exposed, then exactly one dispatch authority is available and the operation's exact command rules remain independently testable.
3. Given the future add-replica migration, when maintainers inspect the documented reuse contract, then they can identify the required operation-specific observation, effect, deadline, terminal-validation, and publication responsibilities without deploying another operational service.

### User Story P2 – Remove pilot-era product surfaces

Narrative: As an operator or contributor, I need configuration, examples, telemetry, and documentation to describe the production behavior rather than a closed comparison experiment.

Independent Test: Search public and deployment surfaces for remove-replica mode selection and pilot terminology and verify only historical measurement context remains where necessary.

Acceptance Scenarios:

1. Given a current or newly generated custom resource, when remove-replica is requested, then no explicit/durable selector is available or required.
2. Given an old pilot checkpoint, when the production workflow loads it, then it returns an explicit incompatible-contract outcome and is never treated as a valid native checkpoint or fresh execution.
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
- **FR-002**: The production workflow contract shall have an explicit version identifier; any prior experimental contract shall produce a typed incompatible-contract outcome and shall not be converted, resumed, or treated as absent. (Stories: P1 recovery, P2 pilot removal)
- **FR-003**: Admission shall validate exact primary and target identity, target non-primary membership, operation mode, quorum/minimum constraints, generation, control version, protocol, epoch, incarnation, UID, and configuration authority. (Stories: P1 single workflow, P1 authority)
- **FR-004**: Temporary absence of exact primary or target status shall produce a wait outcome without operation-generation or status churn. (Stories: P1 authority)
- **FR-005**: Every external command shall be immutable, shall include deterministic operation, action, attempt, primary, target, epoch, incarnation, UID, and configuration authority, shall be validated by an exact deterministic signature, and shall be exposed only after durable preparation. (Stories: P1 recovery, P1 authority)
- **FR-006**: Lost replies, unknown checkpoint-write outcomes, conflicts, and restarts shall recover from authoritative checkpoint and external observation without duplicating an uncertain exposed effect. (Stories: P1 recovery)
- **FR-007**: Pre-commit recovery shall permit only safe continuation or compensation; post-commit recovery shall never roll back the committed reduced topology. (Stories: P1 recovery)
- **FR-008**: Commit evidence shall be explicit, typed, monotonic, and validated against authoritative configuration observations before cleanup or terminal success. (Stories: P1 single workflow, P1 authority)
- **FR-009**: Post-commit completion shall require exact connection absence and retirement evidence; if the admitted target UID still exists, completion shall also require successful UID-fenced label cleanup and UID-fenced deletion, while an already absent admitted UID shall count as satisfied cleanup evidence. (Stories: P1 single workflow, P1 authority)
- **FR-010**: Terminal state shall be durably stored and successfully reloaded before status or topology publication. (Stories: P1 single workflow, P1 compact persistence)
- **FR-011**: Domain rejection, incompatibility, unsafe disposition, compensation, and completion shall remain distinct typed outcomes. (Stories: P1 single workflow, P1 recovery)
- **FR-012**: The workflow shall preserve deterministic replay, bounded redelivery, bounded admission, and conservative quarantine of exposed activities. (Stories: P1 recovery, P1 compact persistence)
- **FR-013**: Active activity inputs and results shall carry compact commands, identifiers, evidence, and state deltas; they shall not repeat complete mutable workflow state or complete configuration descriptors unless a documented safety proof requires the specific field. (Stories: P1 compact persistence)
- **FR-014**: The representative no-fault three-member ScaleDown path shall perform exactly three external effects, no more than two passive observation boundaries, no more than five total durable boundaries, and no more than six accepted checkpoint writes. Any reduction shall still pass restart and ambiguity injection at every remaining boundary. (Stories: P1 compact persistence)
- **FR-015**: Every representative active checkpoint sample shall remain at or below 49,152 encoded bytes, measured from the canonical checkpoint envelope. (Stories: P1 compact persistence)
- **FR-016**: Fault and retry histories shall enforce at most 16 activity records, at most 4,096 decoded bytes per activity input, at most 2,048 decoded bytes per activity result, at most 262,144 encoded bytes per active checkpoint, at most 12,288 encoded bytes per terminal checkpoint, and at most 4,096 decoded bytes per terminal payload. (Stories: P1 compact persistence)
- **FR-017**: Active, terminal, incompatible, rejected, quarantined, conflicted, unknown-write, store-failure, and nondeterministic outcomes shall use one common bounded recovery lifecycle across switchover and remove-replica. (Stories: P2 shared recovery)
- **FR-018**: When one durable observation provides all authority required for the next transition, progression shall continue in the same recovery cycle; when the next external command requires newer cluster, epoch, incarnation, UID, or configuration evidence, progression shall stop and collect a fresh observation before exposing that command. (Stories: P2 shared recovery)
- **FR-019**: Each operation shall retain independently testable rules for collecting observations, validating authority, constructing and dispatching exact effects, calculating deadlines, validating terminal evidence, and publishing final status. (Stories: P2 shared recovery)
- **FR-020**: The old explicit remove implementation shall be deleted only after a traceability matrix maps every inventory item listed in SC-002 to at least one passing production-workflow test and identifies the replacement test by name. (Stories: P1 single workflow, P1 recovery, P1 authority)
- **FR-021**: The migration shall remove remove-specific mode selectors, build-time runtime selection, parity projections, experiment-only telemetry, pilot-only conditions/status, and stale resource and deployment schema. (Stories: P2 pilot removal)
- **FR-022**: General measurement tooling may remain, but remove-specific amortization classifications and thresholds that exist only for the closed comparison hypothesis shall be retired. (Stories: P2 pilot removal)
- **FR-023**: Checkpoint ownership, namespace validation, owner identity, garbage-collection behavior, and separately authorized retained-checkpoint cleanup shall remain enforced. (Stories: P1 compact persistence)
- **FR-024**: The migration shall not change the replica control protocol or replica-agent behavior unless implementation proves that the existing protocol cannot provide a required authority or cleanup proof and obtains an explicit material scope decision. (Stories: P1 authority)
- **FR-025**: Add-replica shall not be ported; documentation shall define the reuse contract by naming the common lifecycle outcomes and the six operation-specific responsibilities from FR-019, and switchover plus remove-replica tests shall demonstrate that separation. (Stories: P2 shared recovery)

### Key Entities

- **Native Remove Execution**: Immutable execution identity, contract version, admitted authority, and compact workflow state for one remove request.
- **Compact Activity**: A bounded durable boundary containing the minimum command, observation, evidence, identifiers, or delta required for deterministic progression.
- **Prepared Command**: An exact immutable external effect durably exposed before dispatch and consumed through a one-use permit.
- **Commit Evidence**: Monotonic proof that the reduced topology crossed the irreversible authority boundary.
- **Cleanup Proof**: Exact evidence that connection, retirement, Kubernetes labels, and deletion satisfy post-commit requirements.
- **Terminal Outcome**: Compact typed completion, compensation, rejection, incompatibility, or unsafe result that is persisted before publication.
- **Operation Reuse Contract**: The boundary between common recovery outcomes and operation-specific observation, authority, effects, deadlines, terminal evidence, and publication.

### Cross-Cutting / Non-Functional

- The workflow shall support the current maximum three-member remove domain unless a separately evidenced change intentionally generalizes it.
- Checkpoint and activity serialization shall be deterministic and reject unknown or incompatible contract shapes.
- Common recovery behavior shall have one production implementation and direct tests rather than generated duplication or hidden per-operation copies.
- Reconciliation shall remain bounded per invocation and return a bounded deadline-aware requeue when work remains.
- Public documentation shall distinguish external effects from passive observations, durable boundaries from accepted writes, and active checkpoint, terminal checkpoint, and terminal payload measurements.

## Success Criteria

- **SC-001**: All remove requests execute through one production durable path, and no CRD, runtime, deployment, or example surface exposes a remove execution-mode selector. (FR-001, FR-021)
- **SC-002**: Before explicit-code deletion, a test traceability matrix names at least one passing replacement test for each of these invariants: pre-commit restart; post-commit restart; lost effect reply; uncertain checkpoint write; conflict reload; authoritative observation recovery; exact primary-status gap without churn; exact target-status gap without churn; commit evidence; configuration authority; UID-fenced label cleanup; UID-fenced deletion; post-commit connection cleanup; incarnation fencing; epoch fencing; corrupt checkpoint handling; incompatible checkpoint handling; terminal-before-status ordering; redelivery without duplicated uncertain effects; three-member admission; active admission; terminal admission; checkpoint owner identity; owner garbage collection; and retained-checkpoint cleanup authorization. (FR-003–FR-012, FR-020, FR-023)
- **SC-003**: Restart tests pass at every pre-commit and post-commit durable boundary with no unsafe rollback or duplicated uncertain effect. (FR-006, FR-007, FR-012)
- **SC-004**: Temporary exact-status gaps produce wait behavior with no status-generation churn. (FR-004)
- **SC-005**: The representative no-fault fixture reports three external effects, two passive observations, five durable boundaries, and no more than six accepted checkpoint writes per run. (FR-014)
- **SC-006**: Across at least three representative no-fault samples, every per-run active checkpoint maximum is at most 49,152 bytes, a minimum 47.6% improvement over the measured 93,841-byte baseline. (FR-013, FR-015)
- **SC-007**: Independent boundary tests reject the seventeenth activity record, a 4,097-byte activity input, a 2,049-byte activity result, a 262,145-byte active checkpoint, a 12,289-byte terminal checkpoint, and a 4,097-byte terminal payload. (FR-016)
- **SC-008**: Old pilot checkpoints produce the incompatible-contract outcome; malformed, corrupt, oversized, or wrong-execution checkpoints produce their defined rejection or quarantine outcomes; none starts a fresh native execution. (FR-002, FR-012)
- **SC-009**: Switchover and native remove-replica produce the common outcomes listed in FR-017 from one production implementation, while separate tests exercise all six operation-specific responsibilities listed in FR-019. (FR-017–FR-019)
- **SC-010**: All existing repository quality checks and default, optional-feature, durable-persistence, operator reconciliation, remove/add/failover lifecycle, checkpoint-measurement, real-provider, and live-cluster regression suites affected by the migration pass without weakening an asserted safety invariant. (FR-020, FR-023)
- **SC-011**: Updated documentation reports representative boundaries, accepted writes, per-run active maxima, terminal checkpoint size, and terminal payload size with correct lifecycle-versus-run-variation labels. (FR-022)
- **SC-012**: No replica control protocol or replica-agent semantic change is present unless separately approved following evidence that a required authority or cleanup proof is unavailable. (FR-024)
- **SC-013**: Documentation names every common lifecycle outcome in FR-017 and every operation-specific responsibility in FR-019, maps both current durable workflows to that contract, and introduces no additional deployed worker, queue, lease, watcher, or scheduler. (FR-025)

## Assumptions

- The prior durable remove path was default-off and experimental, so rejecting its checkpoints is acceptable if incompatibility is explicit and documented.
- The current remove domain remains bounded to three-member configurations.
- The compact 49,152-byte gate is based on the measured 93,841-byte maximum and a conservative 43,509-byte compact-contract projection with approximately 13% headroom.
- Five semantic boundaries remain necessary because fresh observation separates authority-sensitive external effects; accepted writes include the terminal write.
- Existing replica control contracts provide sufficient exact intent, observation, and cleanup evidence.
- Local planning and implementation commits occur directly on the target feature branch; only one final PR is created to `main`.

## Scope

### In Scope

- One common in-process durable recovery implementation.
- Framework-native remove workflow and compact activity/state/terminal contracts.
- Remove checkpoint versioning, incompatibility handling, admission, measurements, and owner/retention behavior.
- Safety traceability and migration of explicit regression coverage.
- Removal of explicit/durable selection and pilot-only production surfaces.
- Switchover adaptation to the shared runner where required to prove reuse.
- Directly affected resource schema, deployment, continuous-integration, examples, tests, and user/operator documentation.
- Stable extension-point documentation for future add-replica migration.

### Out of Scope

- Porting add-replica to the durable framework.
- New generic workers, queues, leases, watchers, distributed ownership, or retry schedulers.
- Active-history compaction or Continue-as-New introduced solely to conceal oversized authoring contracts.
- Changes to replica control protocol or replica-agent semantics without a separately approved protocol-defect decision.
- Generalizing remove beyond the current three-member domain without evidence and explicit scope expansion.
- Optimizing implementation structure solely to improve lexical source-complexity metrics.

## Dependencies

- Existing durable checkpoint, bounded activity, prepared-command, fused-progression, and Kubernetes persistence capabilities.
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
- **Shared-path regression**: Switchover or future workflows could inherit incorrect strict validation. **Mitigation**: keep operation-specific authority rules independently testable and run switchover/add/failover regression suites.

## References

- Specification research completed 2026-09-07.
- Compact checkpoint budget analysis completed 2026-09-07.
- Product direction and safety inventory supplied in the work-item brief dated 2026-09-07.
