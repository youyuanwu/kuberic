# Feature Specification: Framework-Native Remove Replica

**Branch**: `feature/framework-native-remove-replica` | **Created**: 2026-09-07 | **Status**: Approved for Planning
**Input Brief**: Graduate remove-replica from an explicit-versus-durable pilot into the default and only framework-native production workflow.

## Overview

Kuberic operators need remove-replica operations to recover deterministically across controller restarts, ambiguous external outcomes, and concurrent persistence updates without choosing between two execution modes. Remove-replica must operate through one production behavior with one persisted execution contract and one set of safety semantics.

The migration must preserve the exact authority, fencing, cleanup, and publication guarantees already exercised by the existing implementations while removing pilot-only configuration, comparison reporting, telemetry, and documentation. Old experimental execution records may be rejected rather than migrated because the prior path was default-off and the project permits breaking changes, but they must never be silently interpreted as the new production contract.

The production operation must keep durable records compact, bounded, and understandable rather than allowing repeated copies of complete state to dominate persistence. Common recovery behavior must be consistent across supported operations, and the migration must leave a documented, testable reuse pattern for the next operation without adding new operational services.

## Objectives

- Provide a single default remove-replica orchestration path with deterministic durable recovery.
- Preserve all existing safety and lifecycle behavior before deleting the explicit implementation.
- Make common durable recovery outcomes consistent across supported operations while preserving operation-specific authority and safety decisions.
- Reduce the representative active persisted-record maximum from the measured 93,841-byte baseline to at most 49,152 bytes.
- Preserve exactly five representative durable boundaries and six accepted persistence writes.
- Remove comparison-pilot surfaces and document remove-replica as a production migration.
- Demonstrate reuse by supporting both current durable operations through one common recovery behavior and documenting the operation-specific responsibilities required by a future add-replica migration.

## User Scenarios & Testing

### User Story P1 – Remove a replica through one production behavior

Narrative: As a Kuberic operator, I request removal of a non-primary replica and receive the same safe lifecycle behavior without selecting an orchestration mode.

Independent Test: Submit an eligible remove request to a healthy three-member set and verify that it completes, publishes the reduced topology only after durable terminal state, and exposes no mode selector.

Acceptance Scenarios:

1. Given an eligible ScaleDown request, when removal succeeds, then the exact target is retired, disconnected, UID-fenced, deleted, durably terminal, and removed from published topology in that order.
2. Given an eligible Force request, when removal crosses the irreversible commit boundary, then retries and restarts never compensate back to the previous topology.
3. Given an ineligible request, when admission evaluates current authority, then the operation is rejected with an explicit typed domain reason and no unsafe effect is exposed.

### User Story P1 – Recover conservatively from interruption and ambiguity

Narrative: As an operator, I expect controller restarts, lost replies, and uncertain persistence writes to recover from durable evidence without duplicating an uncertain external effect.

Independent Test: Inject restart, lost-reply, conflict, and unknown-write outcomes at every durable boundary and verify deterministic recovery and exact effect counts.

Acceptance Scenarios:

1. Given a restart before commit, when reconciliation resumes, then the operation either continues safely or compensates according to authoritative evidence.
2. Given a restart after commit, when reconciliation resumes, then cleanup completes and the committed topology is never rolled back.
3. Given an exposed effect with a lost reply or uncertain persistence outcome, when the operation reloads, then it isolates and resolves the exposed effect from authoritative observation before allowing another dispatch.
4. Given a concurrent persistence conflict, when the authoritative record is reloaded, then progression resumes from the stored state rather than replaying stale local intent.

### User Story P1 – Preserve authority and fencing guarantees

Narrative: As a cluster owner, I need removal to act only on the intended replica, epoch, incarnation, configuration, and exact owned resource.

Independent Test: Mutate each authority input independently and verify that the operation waits, rejects, or isolates conservatively without status churn or unintended effects.

Acceptance Scenarios:

1. Given temporary loss of exact primary or target status, when reconciliation runs, then it waits without changing operation generation or publishing an invalid terminal result.
2. Given epoch, incarnation, UID, control-version, configuration, or command-signature drift, when a command or observation is validated, then stale authority is rejected.
3. Given cleanup for a replaced owned resource with the same name, when label removal or deletion executes, then UID fencing prevents mutation of the replacement.
4. Given post-commit cleanup, when completion is considered, then exact connection absence and retirement evidence are required.

### User Story P1 – Persist compact bounded execution history

Narrative: As a platform operator, I need durable persistence to remain bounded and operationally inspectable without repeated full-state payloads dominating every persisted record.

Independent Test: Run the representative three-sample no-fault three-member ScaleDown measurement and verify semantic counts, persisted-record limits, and payload classifications.

Acceptance Scenarios:

1. Given the representative no-fault operation, when five durable boundaries complete, then exactly six accepted persistence writes occur.
2. Given any representative sample, when active persisted records are measured using the canonical encoded form, then the per-run maximum is no greater than 49,152 bytes.
3. Given terminalization, when the terminal record is reloaded, then active history is compacted and the full record and terminal payload sizes are reported separately.
4. Given theoretical retry and fault histories, when admission is calculated, then history count, boundary input/output bounds, total encoded capacity, and terminal payload capacity remain explicitly enforced.

### User Story P2 – Maintain consistent reusable durable recovery

Narrative: As a Kuberic maintainer, I need supported durable operations to recover consistently without duplicated control flow or hidden operation-specific safety rules.

Independent Test: Exercise switchover and remove-replica through the common recovery path and verify that both retain their operation-specific safety tests with no duplicated lifecycle control loop.

Acceptance Scenarios:

1. Given active, terminal, rejected, isolated, conflicted, or unknown persistence outcomes, when either supported operation resumes, then it follows the same tested lifecycle outcomes.
2. Given an operation-specific external effect, when it is durably exposed, then exactly one dispatch authority is available and the operation's exact command rules remain independently testable.
3. Given the future add-replica migration, when maintainers inspect the documented reuse contract, then they can identify the required operation-specific observation, effect, deadline, terminal-validation, and publication responsibilities without deploying another operational service.

### User Story P2 – Remove pilot-era product surfaces

Narrative: As an operator or contributor, I need configuration, examples, telemetry, and documentation to describe the production behavior rather than a closed comparison experiment.

Independent Test: Search public and deployment surfaces for remove-replica mode selection and pilot terminology and verify only historical measurement context remains where necessary.

Acceptance Scenarios:

1. Given a current or newly generated custom resource, when remove-replica is requested, then no explicit/durable selector is available or required.
2. Given an old pilot execution record, when production loads it, then it returns an explicit incompatible-contract outcome and is never treated as a valid current record or fresh execution.
3. Given updated documentation, when readers review remove-replica, then it is described as a production framework-native migration with current measurements and stable contracts.

### Edge Cases

- A persisted record is malformed, exceeds bounds, uses an unsupported contract version, or belongs to another execution.
- A terminal record exists while status or topology publication has not yet completed.
- Primary status temporarily omits exact configuration authority even though the operation may later become valid again.
- An effect was exposed but its reply, persistence write, or both are lost.
- A concurrent update reveals a more advanced active or terminal record.
- The target name is reused with another UID during label cleanup or deletion.
- A post-commit target remains connected or has incomplete retirement evidence.
- A retained execution record outlives its owner or is processed by a cleanup identity.
- The current domain exceeds the supported maximum three-member assumption.
- A passive observation cannot safely be fused because fresh authority or fence data is required before the next command.

## Requirements

### Functional Requirements

- **FR-001**: Remove-replica shall have one production execution behavior and shall expose no operator-selectable legacy/current execution mode. (Stories: P1 single workflow, P2 pilot removal)
- **FR-002**: The production execution contract shall have an explicit version identifier; any prior experimental contract shall produce a typed incompatible-contract outcome and shall not be converted, resumed, or treated as absent. (Stories: P1 recovery, P2 pilot removal)
- **FR-003**: Admission shall validate exact primary and target identity, target non-primary membership, operation mode, quorum/minimum constraints, generation, control version, protocol, epoch, incarnation, UID, and configuration authority. (Stories: P1 single workflow, P1 authority)
- **FR-004**: Temporary absence of exact primary or target status shall produce a wait outcome without operation-generation or status churn. (Stories: P1 authority)
- **FR-005**: Every external command shall be immutable, shall include deterministic operation, action, attempt, primary, target, epoch, incarnation, UID, and configuration authority, shall be validated by an exact deterministic signature, and shall be exposed only after durable preparation. (Stories: P1 recovery, P1 authority)
- **FR-006**: Lost replies, unknown persistence-write outcomes, conflicts, and restarts shall recover from the authoritative persisted record and external observation without duplicating an uncertain exposed effect. (Stories: P1 recovery)
- **FR-007**: Pre-commit recovery shall permit only safe continuation or compensation; post-commit recovery shall never roll back the committed reduced topology. (Stories: P1 recovery)
- **FR-008**: Commit evidence shall be explicit, typed, monotonic, and validated against authoritative configuration observations before cleanup or terminal success. (Stories: P1 single workflow, P1 authority)
- **FR-009**: Post-commit completion shall require exact connection absence and retirement evidence; if the admitted target UID still exists, completion shall also require successful UID-fenced label cleanup and UID-fenced deletion, while authoritative absence of the admitted UID shall count as satisfied cleanup evidence. (Stories: P1 single workflow, P1 authority)
- **FR-010**: Terminal state shall be durably stored and successfully reloaded before status or topology publication. (Stories: P1 single workflow, P1 compact persistence)
- **FR-011**: Domain rejection, incompatibility, unsafe disposition, compensation, and completion shall remain distinct typed outcomes. (Stories: P1 single workflow, P1 recovery)
- **FR-012**: Execution shall preserve deterministic replay, at most three redelivery attempts for an admissible exposed effect, bounded admission, and conservative isolation of any uncertain exposed effect. (Stories: P1 recovery, P1 compact persistence)
- **FR-013**: Each durable boundary shall retain only its immutable command, identifiers, evidence, or state change; no boundary input or result shall serialize the complete mutable operation state or a complete multi-configuration descriptor. (Stories: P1 compact persistence)
- **FR-014**: The representative no-fault three-member ScaleDown path shall perform exactly three external effects, exactly two passive observations, exactly five total durable boundaries, and exactly six accepted persistence writes. (Stories: P1 compact persistence)
- **FR-015**: Every representative active persisted-record sample shall remain at or below 49,152 encoded bytes, measured from the canonical encoded execution record. (Stories: P1 compact persistence)
- **FR-016**: Fault and retry histories shall enforce at most 16 history records, at most 4,096 decoded bytes per boundary input, at most 2,048 decoded bytes per boundary result, at most 262,144 encoded bytes per active execution record, at most 12,288 encoded bytes per terminal execution record, and at most 4,096 decoded bytes per terminal payload. (Stories: P1 compact persistence)
- **FR-017**: Active, terminal, incompatible, rejected, isolated, conflicted, unknown-write, persistence-failure, and nondeterministic outcomes shall follow one common bounded recovery behavior across switchover and remove-replica. (Stories: P2 shared recovery)
- **FR-018**: When one durable observation provides all authority required for the next transition, progression shall continue in the same recovery cycle; when the next external command requires newer cluster, epoch, incarnation, UID, or configuration evidence, progression shall stop and collect a fresh observation before exposing that command. (Stories: P2 shared recovery)
- **FR-019**: Each operation shall retain independently testable rules for collecting observations, validating authority, constructing and dispatching exact effects, calculating deadlines, validating terminal evidence, and publishing final status. (Stories: P2 shared recovery)
- **FR-020**: The old explicit remove implementation shall be deleted only after a traceability matrix maps every inventory item listed in SC-002 to at least one passing production-workflow test and identifies the replacement test by name. (Stories: P1 single workflow, P1 recovery, P1 authority)
- **FR-021**: The migration shall remove remove-specific mode selectors, build-time selection, comparison projections, experiment-only telemetry, pilot-only conditions/status, and stale public resource/deployment fields. (Stories: P2 pilot removal)
- **FR-022**: General measurement tooling may remain, but remove-specific amortization classifications and thresholds that exist only for the closed comparison hypothesis shall be retired. (Stories: P2 pilot removal)
- **FR-023**: Persisted-record namespace validation, owner identity, owner-linked garbage-collection behavior, and separately authorized retained-record cleanup shall remain enforced. (Stories: P1 compact persistence)
- **FR-024**: The migration shall not change the replica control protocol or replica-agent behavior unless implementation proves that the existing protocol cannot provide a required authority or cleanup proof and obtains an explicit material scope decision. (Stories: P1 authority)
- **FR-025**: Add-replica shall not be migrated; documentation shall define the reuse contract by naming the common lifecycle outcomes and the six operation-specific responsibilities from FR-019, and switchover plus remove-replica tests shall demonstrate that separation. (Stories: P2 shared recovery)

### Key Entities

- **Native Remove Execution**: Immutable execution identity, contract version, admitted authority, and compact operation state for one remove request.
- **Compact Boundary Record**: A bounded durable boundary containing the minimum command, observation, evidence, identifiers, or state change required for deterministic progression.
- **Prepared Command**: An exact immutable external effect durably exposed before dispatch and consumed through one dispatch authority.
- **Commit Evidence**: Monotonic proof that the reduced topology crossed the irreversible authority boundary.
- **Cleanup Proof**: Exact evidence that connection, retirement, owned-resource labels, and deletion satisfy post-commit requirements.
- **Terminal Outcome**: Compact typed completion, compensation, rejection, incompatibility, or unsafe result that is persisted before publication.
- **Operation Reuse Contract**: The boundary between common recovery outcomes and operation-specific observation, authority, effects, deadlines, terminal evidence, and publication.

### Cross-Cutting / Non-Functional

- The operation shall support exactly the current maximum three-member remove domain.
- Persisted execution and boundary serialization shall be deterministic and reject unknown or incompatible contract shapes.
- Common recovery behavior shall have one production implementation and direct tests rather than generated duplication or hidden per-operation copies.
- Reconciliation shall remain bounded per invocation and return a bounded deadline-aware requeue when work remains.
- Public documentation shall distinguish external effects from passive observations, durable boundaries from accepted writes, and active record, terminal record, and terminal payload measurements.

## Success Criteria

- **SC-001**: All remove requests execute through one production behavior, and no public resource, build, deployment, or example surface exposes a remove execution-mode selector. (FR-001, FR-021)
- **SC-002**: Before explicit-code deletion, a test traceability matrix names at least one passing replacement test for each of these invariants: immutable operation mode; pre-commit restart; post-commit restart; exact prepared command; direct-dispatch authority; one-use dispatch authority; lost effect reply; uncertain persistence write; conflict reload; authoritative observation recovery; exact primary-status gap without churn; exact target-status gap without churn; commit evidence; configuration authority; correlated primary role evidence; correlated lifecycle evidence; UID-fenced label cleanup; UID-fenced deletion; post-commit connection cleanup; incarnation fencing; epoch fencing; bounded redrive of at most three attempts; corrupt record handling; incompatible record handling; distinct unsafe terminal handling; distinct inexact terminal handling; terminal-before-status ordering; redelivery without duplicated uncertain effects; three-member admission; active admission; terminal admission; persisted-record owner identity; owner garbage collection; and retained-record cleanup authorization. (FR-003–FR-012, FR-020, FR-023)
- **SC-003**: Restart tests pass at every pre-commit and post-commit durable boundary with no unsafe rollback or duplicated uncertain effect. (FR-006, FR-007, FR-012)
- **SC-004**: Temporary exact-status gaps produce wait behavior with no status-generation churn. (FR-004)
- **SC-005**: Every representative no-fault sample reports exactly three external effects, two passive observations, five durable boundaries, and six accepted persistence writes. (FR-014)
- **SC-006**: Across at least three representative no-fault samples, every per-run active persisted-record maximum is at most 49,152 bytes, a minimum 47.6% improvement over the measured 93,841-byte baseline. (FR-013, FR-015)
- **SC-007**: Independent boundary tests reject the seventeenth history record, a 4,097-byte boundary input, a 2,049-byte boundary result, a 262,145-byte active execution record, a 12,289-byte terminal execution record, and a 4,097-byte terminal payload. (FR-016)
- **SC-008**: Old pilot records produce the incompatible-contract outcome; malformed, corrupt, oversized, or wrong-execution records produce their defined rejection or isolation outcomes; none starts a fresh production execution. (FR-002, FR-012)
- **SC-009**: Switchover and native remove-replica produce every common outcome listed in FR-017 from one production behavior, while separate tests exercise all six operation-specific responsibilities listed in FR-019. (FR-017–FR-019)
- **SC-010**: All existing quality checks and default, optional-capability, durable-persistence, reconciliation, remove/add/failover lifecycle, persistence-measurement, real-provider, and live-cluster regression suites affected by the migration pass without weakening an asserted safety invariant. (FR-020, FR-023)
- **SC-011**: Updated documentation reports representative boundaries, accepted writes, per-run active maxima, terminal record size, and terminal payload size with correct lifecycle-versus-run-variation labels. (FR-022)
- **SC-012**: No replica control protocol or replica-agent semantic change is present unless separately approved following evidence that a required authority or cleanup proof is unavailable. (FR-024)
- **SC-013**: Documentation names every common lifecycle outcome in FR-017 and every operation-specific responsibility in FR-019, maps both current durable workflows to that contract, and introduces no additional deployed worker, queue, lease, watcher, or scheduler. (FR-025)

## Assumptions

- The prior durable remove path was default-off and experimental, so rejecting its persisted records is acceptable if incompatibility is explicit and documented.
- The current remove domain remains bounded to three-member configurations.
- The compact 49,152-byte gate is based on the measured 93,841-byte maximum and a conservative 43,509-byte compact-contract projection with approximately 13% headroom.
- Five semantic boundaries remain necessary because fresh observation separates authority-sensitive external effects; accepted writes include the terminal write.
- Existing replica control contracts provide sufficient exact intent, observation, and cleanup evidence.
- Local planning and implementation commits occur directly on the target feature branch; only one final PR is created to `main`.

## Scope

### In Scope

- One common in-process durable recovery implementation.
- Framework-native remove execution and compact boundary/state/terminal contracts.
- Remove persisted-record versioning, incompatibility handling, admission, measurements, and owner/retention behavior.
- Safety traceability and migration of explicit regression coverage.
- Removal of explicit/durable selection and pilot-only production surfaces.
- Switchover adaptation to the common recovery behavior where required to prove reuse.
- Directly affected resource schema, deployment, continuous-integration, examples, tests, and user/operator documentation.
- Stable extension-point documentation for future add-replica migration.

### Out of Scope

- Migrating add-replica in this work item.
- New generic workers, queues, leases, watchers, distributed ownership, or retry schedulers.
- Active-history compaction or Continue-as-New introduced solely to conceal oversized authoring contracts.
- Changes to replica control protocol or replica-agent semantics without a separately approved protocol-defect decision.
- Generalizing remove beyond the current three-member domain without evidence and explicit scope expansion.
- Optimizing implementation structure solely to improve lexical source-complexity metrics.

## Dependencies

- Existing durable persistence, bounded-boundary, prepared-command, same-cycle progression, and owned-resource capabilities.
- Existing exact remove intent, action signature, lifecycle observation, connection, retirement, and UID evidence.
- Operator reconciliation and status/topology publication.
- Isolated Kuberic test cluster and repository image workflow for live validation.

## Risks & Mitigations

- **Compatibility ambiguity**: Old pilot records could resemble current state. **Mitigation**: use a new explicit contract version and test fail-closed incompatibility.
- **Stale authority during same-cycle progression**: Progression could schedule a command from outdated observations. **Mitigation**: continue in the same cycle only for proven-safe transitions and require fresh authority before sensitive commands.
- **Over-generalized reuse**: Common behavior could obscure operation safety or encode remove-specific assumptions. **Mitigation**: share only lifecycle outcomes and keep evidence, effects, deadlines, validation, and publication independently testable.
- **Payload regression**: Compact records could gradually reacquire complete state copies. **Mitigation**: enforce the 48 KiB representative gate plus per-boundary and total admission bounds.
- **Coverage loss during deletion**: Explicit tests could disappear without equivalent native assertions. **Mitigation**: maintain a traceability matrix and delete explicit code only after mapped tests pass.
- **Publication race**: Status could report completion before terminal durability. **Mitigation**: require terminal write and terminal reload before publication.
- **Cleanup of replacement objects**: Name reuse could mutate or delete the wrong pod. **Mitigation**: preserve exact UID fencing for labels and deletion.
- **Shared-path regression**: Switchover or future operations could inherit incorrect strict validation. **Mitigation**: keep operation-specific authority rules independently testable and run switchover/add/failover regression suites.

## References

- Specification research completed 2026-09-07.
- Compact persistence-budget analysis completed 2026-09-07.
- Product direction and safety inventory supplied in the work-item brief dated 2026-09-07.
