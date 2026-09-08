---
date: 2026-09-08 16:39:59 UTC
git_commit: 0eb2815c4de0c6d7b070a92b59b929eeb57ce6bd
branch: feature/framework-native-switchover
repository: github.com/youyuanwu/kuberic
topic: "Framework-Native Switchover Graduation Spec Research"
tags: [research, specification]
status: complete
---

# Spec Research: Framework-Native Switchover Graduation

## Summary

The current repository has two switchover execution paths. Newly accepted
switchovers default to the CRD-backed explicit state machine; a default-off
Cargo feature plus `spec.switchoverExecutionMode: durablePilot` selects a
ConfigMap-checkpointed adapter over the same switchover decisions. Both paths
preserve the same topology, epoch, exact-incarnation, correlated-command,
compensation, and terminal-publication rules, but expose different status,
resource, condition, measurement, documentation, test, and CI surfaces.

Framework-native remove-replica has already centralized the workflow-neutral
load/reload, terminal, fused progression, one-use permit, quarantine,
persistence classification, bounded-fuel, and requeue behavior in a shared
runner and ConfigMap provider. Switchover already consumes that runner in its
pilot path. Its remaining operation-specific behavior is observation and
authority collection, exact command preparation, switchover effect and
quarantine interpretation, deadlines, terminal validation, and final topology
publication. The current three-member no-redelivery pilot path is nine external
effects, three passive observations, 12 completed boundaries, and 13 accepted
checkpoint writes.

## Agent Notes

The requested production outcome is one framework-native durable switchover
path with no public execution-mode selector or durable-pilot build feature.
The migration must preserve or strengthen all existing safety properties,
reuse the shared bounded in-process runner and ConfigMap checkpoint provider,
define a switchover-specific versioned contract and bounds, fail closed for
unsupported historical state, and leave add-replica, failover, initial
creation, SQLite issue #42, and unrelated protocol redesign out of scope.

The research must treat
`.paw/work/framework-native-remove-replica/` as read-only precedent and must
cite current code and documentation with exact file and line references.

## Research Findings

### Question 1: What are all current public and internal selectors for explicit versus durable-pilot switchover?

**Answer**:

- The public selector is `spec.switchoverExecutionMode`. It defaults to
  `explicit` and accepts `durablePilot`
  (`kuberic-operator/src/crd.rs:47-58`,
  `kuberic-operator/src/crd.rs:197-217`). The generated/deployed CRD exposes
  the same default and enum
  (`kuberic-operator/deploy/deployment.yaml:90-113`).
- The build selector is the default-off
  `durable-switchover-pilot` Cargo feature
  (`kuberic-operator/Cargo.toml:11-14`). Without that feature, a new
  `durablePilot` request is rejected rather than falling back to explicit
  execution (`kuberic-operator/src/reconciler.rs:234-252`).
- Acceptance checks the resource selector. Pilot acceptance persists
  `status.durableSwitchoverPilot` and clears `status.operation`; explicit
  acceptance persists `status.operation` and clears the pilot reference
  (`kuberic-operator/src/reconciler.rs:1538-1597`).
- Recovery is selected by persisted status, not by rereading the current
  selector: `Phase::Switchover` plus no `status.operation` and a pilot
  reference enters the pilot runner; otherwise it enters the explicit durable
  reconciler (`kuberic-operator/src/reconciler.rs:1788-1816`).
- The pilot module, runtime field, effect bridge, checkpoint decoder branches,
  and tests are feature-gated
  (`kuberic-operator/src/durable/mod.rs:12-22`,
  `kuberic-operator/src/reconciler.rs:43-52`,
  `kuberic-operator/src/reconciler.rs:75-130`,
  `kuberic-operator/src/durable/checkpoint_store.rs:195-239`).
- The kvstore test crate enables the operator pilot feature only as a
  development dependency; its application crate has no corresponding public
  feature (`examples/kvstore/Cargo.toml:6-9`,
  `examples/kvstore/Cargo.toml:21-27`). The checked-in kvstore resource example
  omits the selector, so it uses the explicit default
  (`examples/kvstore/deploy/kubericset.yaml:1-16`).
- CI has a separate switchover-pilot gate whose operator commands pass
  `--features durable-switchover-pilot`; the kvstore development dependency
  supplies the feature for the reconciler test
  (`.github/workflows/CI.yml:52-60`).
- User documentation presents both the explicit example and the optional
  pilot opt-in, build feature, pilot status, and condition
  (`kuberic-operator/README.md:43-86`).
- No operator CLI flag or environment variable selects the switchover engine.
  The operator binary parses neither and constructs one durable client-backed
  reconciler state unconditionally (`kuberic-operator/src/main.rs:1-35`).

**Evidence**: CRD source and deployed schema, Cargo manifests, reconciler
acceptance/recovery routing, operator startup, example manifest, CI, and
operator README at the researched commit.

**Implications**: The old split comprises one CRD spec field, one Cargo
feature, two persisted status forms, feature-gated internal wiring, generated
schema, example/test dependency wiring, dedicated CI commands, and pilot
documentation. There is no separate CLI or environment selector to remove.

### Question 2: How does the explicit switchover state machine establish and validate its safety and recovery behavior?

**Answer**:

- Admission validates the stable snapshot, rejects the current primary or a
  non-member target, increments only the configuration epoch, preserves exact
  member incarnations and quorum, and creates deterministic operation and
  execution identities (`kuberic-operator/src/durable/switchover.rs:32-105`).
- The persisted operation contains previous and target snapshots, old and new
  primary IDs, frozen LSN, secondary cursor, phase/action deadlines, at most
  one pending action, bounded error state, and exact dispatch fences
  (`kuberic-operator/src/crd.rs:352-415`,
  `kuberic-operator/src/crd.rs:794-819`).
- Write revocation is persisted as a deterministic action before dispatch.
  Frozen-LSN capture accepts only the exact old-primary incarnation at the
  previous epoch while it is still primary and write-revoked
  (`kuberic-operator/src/durable/switchover.rs:148-190`).
- Pre-catch-up accepts only the exact target incarnation at the previous epoch,
  in active-secondary role, at or beyond the frozen LSN. Timeout enters
  previous-configuration restoration rather than promotion
  (`kuberic-operator/src/durable/switchover.rs:191-240`).
- Demotion, promotion, retained-member epoch distribution, catch-up
  configuration, write-quorum wait, current configuration, and exact-UID
  routing labels are each separately persisted and correlated
  (`kuberic-operator/src/durable/switchover.rs:242-322`,
  `kuberic-operator/src/durable/switchover.rs:965-1076`).
- Every pending action has a deterministic ID, exact target replica and
  incarnation, expected epoch, and desired postcondition. Recovery validates
  that the action is legal for its phase and reconstructs the same target and
  postcondition (`kuberic-operator/src/durable/switchover.rs:965-1079`,
  `kuberic-operator/src/durable/switchover.rs:1184-1292`).
- Replica actions are accepted as complete only through the exact correlated
  action signature or an exact live postcondition. Live state matching neither
  the precondition nor postcondition poisons the operation
  (`kuberic-operator/src/durable/switchover.rs:580-900`).
- Before every ReplicaAgent dispatch, the reconciler persists agent
  generation, control version, runtime epoch, and the exact action payload.
  Dispatch errors update the durable operation; no execution occurs without a
  persisted pending action and matching action ID
  (`kuberic-operator/src/reconciler.rs:3539-3612`).
- Final success requires every snapshot member to have the exact incarnation,
  target epoch, role, routing label, and target Current configuration.
  Publication then persists the stable snapshot and returns the set to
  `Healthy` (`kuberic-operator/src/durable/switchover.rs:324-340`,
  `kuberic-operator/src/durable/switchover.rs:1145-1182`,
  `kuberic-operator/src/reconciler.rs:3714-3759`).
- Compensation has two paths. Before target promotion, the old primary can
  restore the previous Current configuration. After failed target promotion,
  the old primary is promoted at the new epoch, retained epochs and
  configurations converge, exact labels are restored, and final attestation
  publishes only the validated compensated topology
  (`kuberic-operator/src/durable/switchover.rs:269-285`,
  `kuberic-operator/src/durable/switchover.rs:342-419`,
  `kuberic-operator/src/durable/switchover.rs:1084-1120`).
- Successful and compensated terminals are independently validated against
  the exact target or compensation topology; a pending action or wrong
  topology is rejected (`kuberic-operator/src/durable/switchover.rs:1413-1465`).
- Restart recovery is status-driven. The explicit reconciler reloads
  `status.operation`, revalidates member UIDs before collecting observations,
  observes before deciding, and persists one transition or dispatch fence at a
  time (`kuberic-operator/src/reconciler.rs:3343-3519`).

**Evidence**: Explicit switchover decision logic, CRD operation schema, and the
ordinary durable reconciler.

**Implications**: Graduation must retain the same admission identity, exact
incarnation/epoch/action correlation, write-revocation and frozen-LSN gates,
ordered configuration and label publication, compensation states, terminal
attestation, and observation-first restart behavior.

### Question 3: How does the durable-pilot adapter map the explicit state machine, and which surfaces are duplicated?

**Answer**:

- Pilot input embeds version 2, the kernel execution identity, and the complete
  initial explicit operation. Mutable workflow state is a compact projection
  of phase, frozen LSN, secondary cursor, deadline, pending action, and error
  (`kuberic-operator/src/durable/pilot.rs:68-79`,
  `kuberic-operator/src/durable/pilot.rs:154-204`).
- The workflow replays effect-free explicit decisions in memory. At an
  effect/observation boundary it calls one typed activity, applies and
  validates the returned state, and uses the same explicit terminal validator
  (`kuberic-operator/src/durable/pilot.rs:323-523`).
- `evaluate_adapter_step` directly maps explicit `Persist`, `Wait`,
  `Execute`, exact-label, and `Complete` decisions into pilot observations,
  external prepared effects, waits, or terminals
  (`kuberic-operator/src/durable/pilot.rs:850-925`).
- The resolver converts the logical passive boundary into an exact prepared
  ReplicaAgent or UID-fenced label command, preserving recorded commands on
  replay only when their predecessor and command identity still validate
  (`kuberic-operator/src/durable/pilot.rs:927-1130`).
- The shared runner invokes the adapter to collect exact pods, handles,
  observations, and addressed instances; dispatch through a one-use permit;
  resolve exposed work from observation only; apply deadline policy; and
  validate the loaded terminal
  (`kuberic-operator/src/durable/pilot.rs:1132-1497`,
  `kuberic-operator/src/reconciler.rs:2822-2918`).
- Duplicated public/status surfaces are `status.operation` versus
  `status.durableSwitchoverPilot`, `DurableOperation` versus
  `DurableSwitchoverPilot` conditions, and explicit CRD-state persistence
  versus owner-bound ConfigMap checkpoints
  (`kuberic-operator/src/crd.rs:118-132`,
  `kuberic-operator/src/reconciler.rs:2922-2957`,
  `kuberic-operator/src/reconciler.rs:4141-4157`).
- Duplicated operational surfaces include separate acceptance/publication
  branches, explicit status-write measurements versus pilot checkpoint
  measurements, and separate explicit and pilot reconciler test matrices
  (`kuberic-operator/src/reconciler.rs:1538-1597`,
  `kuberic-operator/src/reconciler.rs:2959-3100`,
  `examples/kvstore/tests/reconciler.rs:3427-3630`,
  `examples/kvstore/tests/reconciler.rs:3630-5165`).

**Evidence**: Pilot contract/workflow/adapter, reconciler routing and
publication, CRD status, and kvstore test harness.

**Implications**: The protocol decision logic is already shared. The split
that remains is execution identity, persistence, conditions, publication
routing, measurements, tests, feature gates, and user-facing schema/docs.

### Question 4: What is already centralized in the shared runner and ConfigMap provider, and what remains switchover-specific?

**Answer**:

- The shared runner owns authoritative checkpoint load and validation,
  terminal short-circuit and reload validation, adapter restore/prepare order,
  fused turn/exposure, bounded host-outcome fuel, one-use dispatch permits,
  quarantine callbacks, fused observation/progression, preparation waits,
  conflict/unknown-write reload, persistence errors, checkpoint disposition,
  observation rejection, nondeterminism, and one-to-ten-second
  deadline-clamped requeues (`kuberic-operator/src/durable/runner.rs:171-494`,
  `kuberic-operator/src/durable/runner.rs:514-598`).
- The shared workflow runtime owns execution/owner/workflow-keyed host caching,
  one process epoch, Kubernetes or in-memory provider construction, exact
  checkpoint-name validation, completed measurement retention, and host
  forgetting (`kuberic-operator/src/durable/workflow_host.rs:18-185`).
- The checkpoint store owns workflow-neutral load/CAS delegation, persistence
  event classification, active/terminal byte measurements, accepted-write
  accounting, and operation-supplied activity/terminal decoders
  (`kuberic-operator/src/durable/checkpoint_store.rs:1-113`,
  `kuberic-operator/src/durable/checkpoint_store.rs:151-260`).
- The Kubernetes provider behavior used by both workflows is same-namespace
  ConfigMap storage with a non-controlling, non-blocking owner reference,
  exact execution-derived object name, and no normal-writer delete
  (`kuberic-operator/src/durable/pilot.rs:1828-1856`,
  `docs/features/kuberic/operator.md:231-262`).
- Switchover retains operation-specific observation collection, snapshot UID
  validation, exact command derivation, explicit decision mapping,
  ReplicaAgent and label effect semantics, quarantine interpretation,
  switchover deadline selection, checkpoint incompatibility classification,
  terminal/accounting validation, conditions, and stable-topology publication
  (`kuberic-operator/src/durable/pilot.rs:850-1497`,
  `kuberic-operator/src/reconciler.rs:2822-3100`).

**Evidence**: Shared runner, workflow host, measured checkpoint store,
switchover adapter, and operator documentation.

**Implications**: Framework-native switchover can use the existing runner and
provider without moving topology policy into them. Its adapter remains the
owner of all switchover-specific authority, effects, recovery interpretation,
terminal proof, and publication.

### Question 5: Which legacy switchover status and checkpoint shapes can exist, how are they recognized, and what fail-closed behavior is established?

**Answer**:

- The explicit persisted shape is `status.operation` with durable operation
  version 1 and kind `Switchover`
  (`kuberic-operator/src/crd.rs:352-415`). Current recovery recognizes it by
  the presence of `status.operation`, independently of the current spec
  selector (`kuberic-operator/src/reconciler.rs:1788-1816`).
- The current pilot persisted shape is
  `status.durableSwitchoverPilot` version 2 with a 32-character lowercase
  execution ID, deterministic checkpoint name, and exact initial-operation
  JSON (`kuberic-operator/src/crd.rs:206-217`,
  `kuberic-operator/src/durable/pilot.rs:1738-1820`).
- Git history shows an earlier deployed pilot contract version 1 at commit
  `a98ffb3`, followed by version 2 at `3f1d9ad`. Both used the same status
  reference concept; the activity contract changed from the original
  explicit-step workflow to the current effect-boundary workflow.
- Kernel checkpoints use envelope format 3, reject unknown envelope fields,
  and distinguish active history from compact terminal state
  (`durable-execution/src/checkpoint.rs:10-122`,
  `durable-execution/src/checkpoint.rs:154-238`).
- Current pilot admission rejects a non-v2 reference, malformed execution ID,
  changed checkpoint name, malformed initial operation, oversized operation,
  unsupported activity version, changed immutable execution contract, and
  invalid terminal shape (`kuberic-operator/src/durable/pilot.rs:586-603`,
  `kuberic-operator/src/durable/pilot.rs:795-843`,
  `kuberic-operator/src/durable/pilot.rs:1738-1884`).
- An unsupported checkpoint envelope format is classified as
  `Incompatible`; other checkpoint validation failures are `Rejected`. The
  reconciler records a blocking condition and does not publish topology or
  dispatch another effect from that reconcile
  (`kuberic-operator/src/durable/pilot.rs:1471-1491`,
  `kuberic-operator/src/reconciler.rs:2898-2918`).
- A persisted pilot reference with no checkpoint is currently treated as an
  accepted execution that has not reached its first checkpoint. This supports
  the documented ordering in which the reference is persisted before
  checkpoint creation or effect dispatch
  (`kuberic-operator/src/reconciler.rs:1553-1584`,
  `kuberic-operator/README.md:55-76`).
- The framework-native remove precedent converts known legacy pilot and
  explicit status atomically into a durable incompatibility marker containing
  source, version, identity, checkpoint name, and fingerprint. It clears
  neither the legacy state as absent nor authorizes a replacement execution
  (`kuberic-operator/src/reconciler.rs:289-395`,
  `docs/features/kuberic/implemented/agent-owned-replica-remove-protocol.md:120-130`).

**Evidence**: Current CRD and pilot validators, kernel checkpoint schema,
reconciler routing, operator documentation, remove-replica precedent, and
repository history for pilot v1/v2.

**Implications**: Compatibility handling must distinguish at least explicit
operation v1, pilot reference/workflow v1, current pilot
reference/workflow v2, malformed references, unsupported format-3 execution
contracts, and missing checkpoints. Unsupported historical state cannot be
treated as no active execution, selector fallback, or permission to start a
new switchover.

### Question 6: Does the ReplicaAgent protocol already preserve current safety, and what does an agent-owned coarse intent change?

**Answer**:

- The current protocol is sufficient to execute the existing operator-owned
  sequence safely. Correlated control v3 validates action ID, exact target
  replica/incarnation, agent generation, input signature, control version,
  observed runtime epoch, and single active-agent work; duplicates with the
  same identity replay their observation and conflicts are rejected
  (`kuberic-core/src/replica_agent.rs:548-667`).
- The action vocabulary already contains every fine-grained switchover
  primitive: write revocation, role change, epoch update, catch-up
  configuration, write-quorum wait, and Current configuration
  (`kuberic-core/src/types.rs:448-489`). The operator separately owns exact-UID
  label changes.
- Agent status exposes one current action and 16 retained terminal
  observations. The ledger is generation-qualified, bounded, volatile, and
  is not treated as global workflow history or exactly-once execution
  (`docs/features/kuberic/status.md:40-62`,
  `docs/features/kuberic/protocols.md:145-151`).
- The current switchover checkpoint compensates for that bounded local ledger
  by persisting every action identity and observing both the ledger and live
  postconditions before retry or advancement
  (`docs/features/kuberic/protocols.md:118-166`).
- There is no `SwitchoverIntent`, switchover coordinator progress, or
  switchover terminal result in the current ReplicaAgent contract. Only
  add/build and remove have coarse intents
  (`kuberic-core/src/types.rs:448-456`).
- Repository status and the implemented remove ADR record agent-owned
  switchover as the next separate ownership candidate; today its revoke,
  catch-up, demotion, promotion, epoch, configuration, and compensation
  sequence remains operator-owned
  (`docs/features/kuberic/status.md:163-168`,
  `docs/features/kuberic/implemented/agent-owned-replica-remove-protocol.md:279-285`).

**Evidence**: ReplicaAgent admission/replay behavior, action vocabulary,
protocol/status documentation, and the implemented remove ownership ADR.

**Implications**: No new primitive is required merely to preserve the current
fine-grained safety behavior. A coarse intent is required only to change the
ownership boundary so one agent, rather than the operator checkpoint, owns the
local switchover sequence. That coarse contract does not exist today and would
remain separate from add and remove semantics.

### Question 7: Which add/remove contract and recovery patterns are reusable for a separate switchover intent?

**Answer**:

- Add/build demonstrates one versioned operator-to-primary intent over the
  existing correlated control RPC, with exact primary/target identities,
  generations, endpoints, epoch, previous/catch-up/current configurations,
  minimum membership, operation deadlines, and compensation deadline
  (`kuberic-core/src/add_replica.rs:1-137`,
  `docs/features/kuberic/implemented/agent-owned-replica-add-build-protocol.md:28-58`).
- Remove demonstrates the stricter current admission shape: intent protocol
  version, operation/action/attempt identity, input signature, exact primary
  generation and control version, optional exact target peer authority,
  structural configuration fences, quorum/minimum constraints, bounded
  attempts, call timeout, overall deadline, compensation cap, and target
  retirement timeout (`kuberic-core/src/remove_replica.rs:13-121`).
- Both reuse correlated control v3 duplicate replay, generation fencing,
  one-current-action admission, and bounded current/terminal status instead of
  adding another operator-to-agent RPC
  (`kuberic-core/src/replica_agent.rs:548-667`,
  `docs/features/kuberic/implemented/agent-owned-replica-remove-protocol.md:46-72`).
- Both expose operation-specific coordinator progress and typed terminal
  results while keeping CRD/checkpoint state as durable global authority. Agent
  state remains bounded and volatile
  (`docs/features/kuberic/implemented/agent-owned-replica-add-build-protocol.md:28-44`,
  `docs/features/kuberic/implemented/agent-owned-replica-remove-protocol.md:30-44`).
- Add compensation restores previous configuration, removes the exact target
  connection, and requests target cleanup only before commit; remove
  compensation restores previous Current only before reduced Current is
  observed. Both forbid rollback after their operation-specific irreversible
  Current-configuration boundary
  (`docs/features/kuberic/implemented/agent-owned-replica-add-build-protocol.md:75-85`,
  `docs/features/kuberic/implemented/agent-owned-replica-remove-protocol.md:132-161`).
- The existing lifecycle peer is intentionally limited to add/build stages and
  remove retirement; it is not a general reconfiguration API
  (`docs/features/kuberic/implemented/agent-owned-replica-add-build-protocol.md:100-116`).

**Evidence**: Current add/remove core contracts, ReplicaAgent admission, and
implemented protocol ADRs.

**Implications**: Reusable behavior is the existing correlated control RPC,
versioned signed intent, generation/control-version admission, bounded
operation-specific progress/result status, duplicate replay, pre-commit-only
compensation, and durable operator checkpoint authority. No current contract
demonstrates agent-to-agent switchover coordination or a switchover peer stage.

### Question 8: What is the canonical current no-fault switchover path and measurement?

**Answer**:

- For a three-member set, the no-redelivery pilot completes nine external
  effects: revoke old-primary writes; demote the old primary; promote the
  target; update the third member's epoch; install catch-up configuration;
  wait for write quorum; install Current configuration; label the target
  primary; and label the old primary secondary
  (`kuberic-operator/src/durable/pilot.rs:2052-2072`).
- It records three passive observations: frozen-LSN capture, target catch-up,
  and final topology attestation
  (`kuberic-operator/src/durable/pilot.rs:2052-2072`).
- The total is 12 completed durable boundaries and 13 accepted checkpoint
  writes, including terminal persistence
  (`docs/features/kuberic/testing.md:410-429`,
  `kuberic-operator/src/durable/pilot.rs:4428-4503`).
- Six documented runs observed maximum active checkpoints from 31,597 to
  31,605 bytes and terminal checkpoints from 4,077 to 4,081 bytes. These are
  run-specific measurements, not compatibility constants
  (`docs/features/kuberic/testing.md:414-420`).
- The terminal payload has a 4,096-byte declared ceiling and carries exact
  external-effect/passive-observation accounting, but the current documented
  measurement output does not publish its exact no-fault byte count
  (`kuberic-operator/src/durable/pilot.rs:72-76`,
  `kuberic-operator/src/durable/pilot.rs:2104-2187`,
  `examples/kvstore/tests/reconciler.rs:3720-3788`).
- The explicit path performs the same logical mutation sequence but stores its
  transitions in CRD status, so checkpoint loads, checkpoint writes, durable
  boundary counts, and checkpoint bytes are reported as not applicable
  (`examples/kvstore/tests/reconciler.rs:3500-3575`).

**Evidence**: Pilot transcript projection, unit measurement, kvstore
integration measurement, and testing documentation.

**Implications**: The current behavioral baseline is exactly 9 external
effects, 3 passive observations, 12 completed boundaries, and 13 accepted
writes. Active and terminal checkpoint bytes are measured separately; exact
terminal-payload bytes are not currently recorded.

### Question 9: Which maximum-fault histories and dimensions are currently modeled?

**Answer**:

- The full success history models the base no-fault sequence plus at most one
  proven-no-admission redelivery for each of seven ReplicaAgent effects. For
  three members it permits at most 19 completed activities, nine base external
  effects, and three required passive observations
  (`kuberic-operator/src/durable/pilot.rs:1923-2072`,
  `kuberic-operator/src/durable/pilot.rs:4506-4537`).
- The longest rollback history models revoke, frozen-LSN observation, target
  catch-up observation, demotion, failed/uncertain target promotion,
  old-primary re-promotion, epoch convergence for all non-old-primary members,
  compensation catch-up and Current configurations, two label outcomes, and
  final compensation attestation. With one redelivery slot per eight
  ReplicaAgent effects, it permits at most 21 activities, ten possible
  external effects, three required passive observations, and two label
  boundaries that may resolve externally or passively
  (`kuberic-operator/src/durable/pilot.rs:2189-2210`,
  `kuberic-operator/src/durable/pilot.rs:2286-2307`,
  `kuberic-operator/src/durable/pilot.rs:4506-4537`).
- Shorter compensation histories separately model failure before demotion and
  failure after demotion but before target promotion
  (`kuberic-operator/src/durable/pilot.rs:2075-2094`).
- Persistence conflict, applied/unapplied unknown outcomes, exposed-effect
  quarantine, lost replies, stale target incarnation, terminal CAS conflict,
  failed status publication, and restart after every turn are modeled by the
  pilot integration matrix
  (`docs/features/kuberic/testing.md:390-408`,
  `examples/kvstore/tests/reconciler.rs:3877-5128`).
- Current pilot limits are three replicas, 32 activity records, 64 workflow
  transitions, 3,000-byte workflow input, 8,192-byte activity input,
  4,096-byte result, 4,096-byte terminal payload, and one shared 770,048-byte
  active/terminal encoded checkpoint ceiling
  (`kuberic-operator/src/durable/pilot.rs:68-76`).
- Current error text is bounded to 512 characters
  (`kuberic-operator/src/durable/mod.rs:37-39`,
  `kuberic-operator/src/durable/pilot.rs:1709-1721`).

**Evidence**: Pilot transcript/accounting projections, bounds constants,
integration ambiguity matrix, and error normalization.

**Implications**: Independent production bounds must account separately for
the longest success and compensation histories, bounded redelivery, passive
and externally resolved label outcomes, input/result variants, active and
terminal lifecycle sizes, terminal payload/accounting, error text, and pure
transition plus host-outcome fuel. The current pilot uses a shared
active/terminal checkpoint ceiling and therefore does not provide independent
production values for those dimensions.

### Question 10: Which tests prove current switchover behavior, and what replacement gaps exist?

**Answer**:

| Behavior | Current evidence |
|---|---|
| Explicit success and topology publication | `test_reconciler_switchover` (`examples/kvstore/tests/reconciler.rs:3427`) |
| Pilot success, exact 9/3/12/13 accounting, stable publication | `test_durable_execution_switchover_pilot_happy_path` (`examples/kvstore/tests/reconciler.rs:3630`) |
| Persistence outcomes and unknown-effect behavior | `test_durable_execution_switchover_pilot_fr017_operation_outcome_matrix` and `...unknown_checkpoint_outcomes_requeue_without_effect` (`examples/kvstore/tests/reconciler.rs:3877`, `examples/kvstore/tests/reconciler.rs:5024`) |
| Fresh-fence redelivery and exact command identity | `...fr019_deadline_policy_preserves_fresh_fence_redelivery`, `...observes_every_lost_reply_once`, and unit prepared-command validation (`examples/kvstore/tests/reconciler.rs:4282`, `examples/kvstore/tests/reconciler.rs:4670`, `kuberic-operator/src/durable/pilot.rs:615-734`) |
| Restart recovery | Pilot every-turn restart and explicit every-boundary state loss (`examples/kvstore/tests/reconciler.rs:4418`, `examples/kvstore/tests/reconciler.rs:5190`) |
| Compensation | Pilot failed-promotion publication and explicit failed-promotion/write restoration (`examples/kvstore/tests/reconciler.rs:4502`, `examples/kvstore/tests/reconciler.rs:5497`, `examples/kvstore/tests/reconciler.rs:5589`) |
| Exact UID and generation fencing | Stale target incarnation rejection, exact snapshot-UID label dispatch, and dispatch-generation replay tests (`examples/kvstore/tests/reconciler.rs:4951`, `kuberic-operator/src/reconciler.rs:5196-5327`, `kuberic-operator/src/reconciler.rs:5952-6049`) |
| Quarantine after restart | `exposed_activity_is_quarantined_after_host_restart` (`kuberic-operator/src/durable/pilot.rs:2786`) |
| Terminal reload and publication retry | Unit terminal reload, integration reload-before-publication, and terminal CAS conflict (`kuberic-operator/src/durable/pilot.rs:2682`, `examples/kvstore/tests/reconciler.rs:4755`, `examples/kvstore/tests/reconciler.rs:4865`) |
| Terminal identity/topology validation | `loaded_terminal_revalidates_identity_topology_and_outcome_kind` and shared explicit terminal validation (`kuberic-operator/src/durable/pilot.rs:3605`, `kuberic-operator/src/durable/switchover.rs:1695-1713`) |
| Activity/fuel bounds and accounting reachability | `maximum_projected_history_fits_both_budgets`, `success_and_rollback_transcripts_fit_with_redelivery_headroom`, and terminal-accounting projection tests (`kuberic-operator/src/durable/pilot.rs:2529`, `kuberic-operator/src/durable/pilot.rs:3810`, `kuberic-operator/src/durable/pilot.rs:4506`) |

Current replacement gaps are factual absences:

- No selector-free `framework_native_switchover` routing or integration test
  exists.
- No switchover legacy-status incompatibility-marker test exists.
- No independent one-byte-over matrix exists for switchover activity count,
  input, result, active checkpoint, terminal checkpoint, and terminal payload.
- No `SwitchoverIntent` admission, duplicate replay, generation restart,
  progress, compensation, or terminal-result tests exist because that
  contract is absent.
- No real-cluster `kuberic-tests` switchover checkpoint/owner/publication test
  exists; the current switchover coverage is operator unit plus kvstore
  high-fidelity reconciler coverage.

**Evidence**: Named operator and kvstore tests plus absence of
`framework_native_switchover`, `SwitchoverIntent`, and switchover tests under
`kuberic-tests/`.

**Implications**: Existing safety tests define the regression inventory.
Legacy explicit/pilot tests cannot be removed until the sole path covers the
same success, compensation, recovery, ambiguity, identity, publication,
terminal, and bound behaviors, plus compatibility and any new coarse-intent
contract behavior.

### Question 11: Which repository commands and CI gates are authoritative?

**Answer**:

- CI runs:
  `cargo check --all-targets`,
  `cargo fmt --all -- --check`,
  `cargo clippy --all-targets -- -D warnings`,
  the targeted switchover and remove measurement gates,
  `cargo build --all-targets`,
  `just images` after KinD creation, and
  `cargo test --all --all-features`
  (`.github/workflows/CI.yml:41-88`).
- Current switchover-specific CI commands are:
  `cargo test -p kuberic-operator --features durable-switchover-pilot success_and_rollback_transcripts_fit_with_redelivery_headroom`,
  `... maximum_projected_history_fits_both_budgets`,
  `... measurements_ -- --nocapture`, and
  `cargo test -p kvstore --test reconciler test_durable_execution_switchover_pilot_happy_path -- --nocapture`
  (`.github/workflows/CI.yml:52-60`).
- The documented complete local switchover matrix is:
  `CARGO_BUILD_JOBS=2 CARGO_INCREMENTAL=0 RUST_MIN_STACK=4194304 cargo test -p kvstore --test reconciler test_durable_execution_switchover_pilot_ -- --nocapture`
  (`docs/features/kuberic/testing.md:390-457`).
- The meaningful non-cluster and documentation suites are:
  `cargo test -p kuberic-core -p kuberic-operator -p kvstore -p sqlite-replicated`
  and `cargo test --doc --workspace`
  (`docs/features/kuberic/testing.md:198-220`).
- The deterministic Kubernetes-provider gate is
  `cargo test -p kuberic-durable-execution --features kubernetes --test kubernetes_checkpoint`;
  the conditional authorized real-API gate is
  `cargo test -p kuberic-durable-execution --features kubernetes --test kubernetes_checkpoint_real -- --nocapture`
  (`docs/features/kuberic/testing.md:450-486`).
- Container-image build/deploy behavior is defined by `just images` and the
  operator deployment recipe (`justfile:1-39`).
- There is no standalone CRD generation command. CRD tests compare
  `KubericSet::crd()` content with required fields in the checked-in deployment
  manifest, so schema changes are validated through operator tests while
  `deployment.yaml` remains a checked-in artifact
  (`kuberic-operator/src/crd.rs:1083-1137`).

**Evidence**: CI workflow, testing guide, justfile, and CRD schema tests.

**Implications**: Removing the pilot feature/selector changes the dedicated
switchover CI commands, all-features surface, Cargo manifests, CRD parity
tests, and generated deployment schema. The shared runner/provider and
workspace-wide gates remain authoritative.

### Question 12: Which documentation and generated artifacts describe the old split or must record the graduated behavior?

**Answer**:

- Public schema and deployment artifacts:
  `kuberic-operator/src/crd.rs`,
  `kuberic-operator/deploy/deployment.yaml`,
  `kuberic-operator/Cargo.toml`,
  and `examples/kvstore/Cargo.toml`.
- User/operator documentation:
  `kuberic-operator/README.md` explicitly documents the selector, build
  feature, pilot status, condition, ConfigMap behavior, and measurements
  (`kuberic-operator/README.md:43-90`).
- Architecture/protocol documentation:
  `docs/features/kuberic/operator.md` describes explicit status recovery,
  shared runner ownership, the pilot selection model, checkpoint lifecycle,
  and terminal publication (`docs/features/kuberic/operator.md:210-342`);
  `docs/features/kuberic/protocols.md` describes the explicit sequence and
  feature-gated replay path (`docs/features/kuberic/protocols.md:118-166`).
- Test and measurement documentation:
  `docs/features/kuberic/testing.md` records the pilot matrix, exact 9/3/12/13
  accounting, run-specific byte ranges, bounds, and commands
  (`docs/features/kuberic/testing.md:381-457`).
- Roadmap/status documentation:
  `docs/features/kuberic/durable-execution-roadmap.md` records optional
  switchover as a shared-runner consumer and keeps its limits
  operation-specific (`docs/features/kuberic/durable-execution-roadmap.md:169-184`);
  `docs/features/kuberic/status.md` records explicit CRD recovery and
  agent-owned switchover as an open candidate
  (`docs/features/kuberic/status.md:24-35`,
  `docs/features/kuberic/status.md:163-168`).
- Safety history:
  `docs/features/kuberic/design-gaps.md` records the currently fixed explicit
  restart, revoke, compensation, and fail-closed behaviors
  (`docs/features/kuberic/design-gaps.md:43-115`,
  `docs/features/kuberic/design-gaps.md:901-918`).
- Related future design:
  `docs/features/kuberic/rolling-upgrade-design.md` refers to composition with
  the existing durable switchover state machine and its frozen-LSN catch-up
  boundary (`docs/features/kuberic/rolling-upgrade-design.md:515-518`,
  `docs/features/kuberic/rolling-upgrade-design.md:802-812`).
- The implemented add/remove ADRs establish the unchanged operation separation,
  correlated control, compensation, and coarse-intent precedent
  (`docs/features/kuberic/implemented/agent-owned-replica-add-build-protocol.md:28-58`,
  `docs/features/kuberic/implemented/agent-owned-replica-remove-protocol.md:73-130`,
  `docs/features/kuberic/implemented/agent-owned-replica-remove-protocol.md:279-285`).
- CI is also a generated operational record of the old feature-gated split
  (`.github/workflows/CI.yml:52-60`).

**Evidence**: Current checked-in schema, manifests, Cargo configuration,
operator/protocol/testing/roadmap/status/design documentation, implemented
ADRs, and CI.

**Implications**: Graduation documentation must record one default execution
path, its independent versioned contract and bounds, measured no-fault and
maximum-fault behavior, compatibility handling, terminal/publication behavior,
and the unchanged scope of create, add, remove, and failover. Historical
design-gap and ADR text remains useful as behavioral precedent but must not
misstate the current execution selector or ownership boundary.

## Open Unknowns

- **Exact current no-fault switchover terminal-payload bytes**: the integration
  test calculates and bounds the payload but does not print it, and the testing
  guide records only active and terminal checkpoint byte ranges.
- **Exact independent production switchover bounds**: the current contract
  supplies pilot limits and transcript maxima, but no framework-native compact
  switchover contract exists from which independent input, result, active,
  terminal, payload, error, and fuel values can be measured.
- **Agent-to-agent coordination for a coarse switchover intent**: the
  repository has no switchover intent or peer stage. The existing lifecycle
  peer is deliberately limited to add/build and remove retirement, so current
  behavior does not establish how one agent would coordinate the old primary,
  target, and retained members without a separate operation-specific contract.
- **Live prevalence of historical pilot v1/v2 checkpoints**: repository
  history proves both contract versions could have been deployed, but the
  repository cannot determine whether any current clusters retain them.

## User-Provided External Knowledge (Manual Fill)

No external/manual research questions were provided.
