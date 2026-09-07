---
date: 2026-09-07 04:03:21 UTC
git_commit: 740ee5119327d21ab5d7987bb1fab0df522096be
branch: feature/durable-remove-replica
repository: kuberic
topic: "Durable Remove Replica Spec Research"
tags: [research, specification]
status: complete
---

# Spec Research: Durable Remove Replica

## Summary

The current remove-replica workflow is an explicit CRD-status state machine. It
is selected by healthy scale-down or stale/dead/missing-secondary recovery,
persists a version-2 operation before mutation, delegates one coarse
`RemoveReplicaIntent` to the exact primary, records reduced-Current commit
evidence before cleanup, and publishes the reduced stable topology only after
UID-fenced retirement and deletion.

The switchover pilot already supplies reusable kernel primitives for typed
activities, prepared effect exposure, fused progression, bounded admission,
terminal compaction, and ConfigMap CAS storage. Its operator-facing workflow,
status, telemetry, feature gate, action whitelist, and reconciliation bridge
remain switchover-specific. The current feasibility result is conditionally
feasible because its runtime-neutrality test scans the entire manifest and
mistakes the `tokio` dev-dependency for a library dependency.

## Agent Notes

The work ports `remove_replica` as the second workflow hosted by the experimental
durable-execution kernel. The existing explicit remove-replica path must remain
the default, and the new path must be protected by an explicit default-off Cargo
feature. The work must preserve the kernel's safety, determinism, admission, and
terminal-ordering contracts while producing honest complexity and checkpoint
measurements. Documentation must also correct the stale feasibility predicate
and post-PR-48 roadmap claims.

All review and plan-generation activities use single-model `gpt-5.6-sol`.

## Research Findings

### Question 1: Where and how is the current explicit remove-replica workflow selected, initialized, advanced, persisted, resumed, and surfaced through status?

**Answer**:

- Selection has two entry classes. Healthy scale-down chooses the highest-ID
  stable secondary after target pre-admission and starts `ScaleDown`
  ([kuberic-operator/src/reconciler.rs:1477-1518](../../../kuberic-operator/src/reconciler.rs#L1477-L1518)).
  Missing, stale, or unrecoverable old secondary incarnations start `Force`
  removal ([kuberic-operator/src/reconciler.rs:771-816](../../../kuberic-operator/src/reconciler.rs#L771-L816),
  [kuberic-operator/src/reconciler.rs:1028-1079](../../../kuberic-operator/src/reconciler.rs#L1028-L1079),
  [kuberic-operator/src/reconciler.rs:1280-1329](../../../kuberic-operator/src/reconciler.rs#L1280-L1329)).
- Initialization validates the stable snapshot, non-primary active-secondary
  target, exact target incarnation/Pod UID, `minReplicas`, and retained previous
  write quorum; it derives the reduced snapshot and persists phase
  `RemoveFreezeIntent` with operation version 2, mode, identities, endpoint, and
  deadlines ([kuberic-operator/src/durable/remove_replica.rs:51-146](../../../kuberic-operator/src/durable/remove_replica.rs#L51-L146)).
- Advancement reloads `status.operation`, gathers exact Pod/agent observations,
  invokes `decide_remove_replica`, and executes one returned decision per
  reconcile ([kuberic-operator/src/reconciler.rs:3274-3485](../../../kuberic-operator/src/reconciler.rs#L3274-L3485)).
  The decision function separates pre-commit intent freezing/coordination from
  post-commit cleanup based on durable commit evidence
  ([kuberic-operator/src/durable/remove_replica.rs:152-229](../../../kuberic-operator/src/durable/remove_replica.rs#L152-L229)).
- Every state transition, dispatch fence, error, commit record, cleanup fact,
  snapshot publication, and terminal status is written through
  `KubericSet.status.operation`; side effects are issued only from a persisted
  decision ([kuberic-operator/src/reconciler.rs:3487-3735](../../../kuberic-operator/src/reconciler.rs#L3487-L3735)).
- Resume is CRD-driven: `Phase::RemovingReplica` always re-enters
  `reconcile_durable_operation`, which reloads the persisted operation rather
  than relying on process-local driver state
  ([kuberic-operator/src/reconciler.rs:1646-1656](../../../kuberic-operator/src/reconciler.rs#L1646-L1656),
  [kuberic-operator/src/reconciler.rs:3274-3288](../../../kuberic-operator/src/reconciler.rs#L3274-L3288)).
- Status exposes the complete operation, frozen intent, commit evidence,
  cleanup evidence, and typed poisoned disposition
  ([kuberic-operator/src/crd.rs:296-499](../../../kuberic-operator/src/crd.rs#L296-L499)).
  The `DurableOperation` condition reports freezing, primary coordination,
  compensated finalization, committed-removal publication, completion,
  safe failure, or poison ([kuberic-operator/src/durable/mod.rs:154-275](../../../kuberic-operator/src/durable/mod.rs#L154-L275)).

**Evidence**: Current reconciler, remove decision state machine, CRD status
schema, and operation-condition mapping.

**Implications**: A kernel-hosted option must be a separate explicit selection
path while the existing `status.operation` path remains the default and remains
resumable without any kernel checkpoint.

### Question 2: What are the complete externally visible commands, authoritative observations, state transitions, fencing checks, retry/unknown-outcome policies, quarantine behavior, and terminal-ordering guarantees of the explicit remove-replica workflow?

**Answer**:

- The operator sends one coarse correlated `RemoveReplicaIntent` to the exact
  current primary. The primary coordinator installs reduced CatchUp over the
  previous configuration, waits for write catch-up quorum, marks
  current-install dispatch, installs reduced Current, removes the exact old
  connection, requests target `Retire`, and attests the result
  ([docs/features/kuberic/protocols.md:259-284](../../../docs/features/kuberic/protocols.md#L259-L284),
  [kuberic-core/src/replica_agent.rs:2332-2573](../../../kuberic-core/src/replica_agent.rs#L2332-L2573),
  [kuberic-core/src/replica_agent.rs:2831-3015](../../../kuberic-core/src/replica_agent.rs#L2831-L3015)).
  After commit, the operator performs an exact-UID `role=retired` patch,
  exact-UID Pod deletion, stable-snapshot publication, and completion
  ([kuberic-operator/src/durable/remove_replica.rs:808-1017](../../../kuberic-operator/src/durable/remove_replica.rs#L808-L1017)).
- Authoritative observations are exact primary runtime identity, health,
  writable epoch and configuration, correlated current/retained action ledger,
  coordinator progress/result, active connection identity, target lifecycle
  state, Pod UID, and role label
  ([kuberic-operator/src/durable/remove_replica.rs:384-508](../../../kuberic-operator/src/durable/remove_replica.rs#L384-L508),
  [kuberic-operator/src/durable/remove_replica.rs:826-955](../../../kuberic-operator/src/durable/remove_replica.rs#L826-L955)).
  Agent progress is volatile; CRD commit evidence and snapshots are global
  durable authority ([docs/features/kuberic/operator.md:398-415](../../../docs/features/kuberic/operator.md#L398-L415)).
- The durable operator phases are
  `RemoveFreezeIntent → RemoveDispatchIntent → RemoveAwaitCoordination →
  RemoveRecordCommit → RemoveAwaitCleanup → RemoveDeleteTargetPod →
  RemovePublishTopology → RemoveFinalize → Completed`, with
  `RemoveCompensateFinalize → Failed` for proven safe compensation and
  `Poisoned` for unsafe ambiguity
  ([kuberic-operator/src/crd.rs:680-693](../../../kuberic-operator/src/crd.rs#L680-L693),
  [kuberic-operator/src/durable/remove_replica.rs:179-229](../../../kuberic-operator/src/durable/remove_replica.rs#L179-L229),
  [kuberic-operator/src/durable/remove_replica.rs:818-1017](../../../kuberic-operator/src/durable/remove_replica.rs#L818-L1017)).
- Admission and dispatch fences include non-primary stable-secondary identity,
  exact incarnation/Pod UID, retained quorum and minimum, exact primary
  incarnation/generation/control version/runtime epoch, exact target generation
  for `ScaleDown`, protocol versions, signed input/configuration identities, and
  frozen deadlines ([kuberic-operator/src/durable/remove_replica.rs:51-104](../../../kuberic-operator/src/durable/remove_replica.rs#L51-L104),
  [kuberic-operator/src/durable/remove_replica.rs:234-371](../../../kuberic-operator/src/durable/remove_replica.rs#L234-L371),
  [kuberic-operator/src/durable/remove_replica.rs:1312-1403](../../../kuberic-operator/src/durable/remove_replica.rs#L1312-L1403)).
  `ScaleDown` requires reachable lifecycle-peer-v2 authority and never changes
  to `Force`; `Force` permits absent target authority but retains the same
  topology and quorum fences ([docs/features/kuberic/protocols.md:315-325](../../../docs/features/kuberic/protocols.md#L315-L325)).
- Dispatch is observation-first and correlated. Precondition/continuity
  rejection clears frozen dispatch evidence for refresh; `AgentBusy` retains
  it for retry; conflict poisons; other transport or unknown errors only record
  an error and rely on the agent ledger/postcondition on the next reconcile
  ([kuberic-operator/src/durable/effects.rs:520-589](../../../kuberic-operator/src/durable/effects.rs#L520-L589)).
  A new primary-agent generation can redrive only from exact previous Current
  or reduced CatchUp, with at most three pre-commit attempts
  ([kuberic-operator/src/durable/remove_replica.rs:510-600](../../../kuberic-operator/src/durable/remove_replica.rs#L510-L600)).
- The explicit path has no kernel `Quarantined` host outcome. Its quarantine
  equivalent is durable `Poisoned` status with one of
  `FailedPreCommitIncomplete`, `InvalidRemovalState`, or
  `AmbiguousPrimaryRestart`; none authorizes inferred commit, rollback, or
  cleanup ([kuberic-operator/src/crd.rs:476-499](../../../kuberic-operator/src/crd.rs#L476-L499),
  [docs/features/kuberic/protocols.md:317-325](../../../docs/features/kuberic/protocols.md#L317-L325)).
- Exact reduced Current is irreversible. Before current-install dispatch,
  exact previous Current can complete compensation; after dispatch, ambiguous
  state poisons rather than restoring previous Current
  ([kuberic-core/src/replica_agent.rs:2592-2678](../../../kuberic-core/src/replica_agent.rs#L2592-L2678)).
  Commit evidence and the reduced workflow-scoped `committedSnapshot` are
  persisted before cleanup; the reduced `stableSnapshot` is published only
  after connection absence, terminal retirement evidence, UID-fenced label,
  and UID-fenced deletion are established
  ([kuberic-operator/src/durable/remove_replica.rs:766-802](../../../kuberic-operator/src/durable/remove_replica.rs#L766-L802),
  [kuberic-operator/src/durable/remove_replica.rs:808-1017](../../../kuberic-operator/src/durable/remove_replica.rs#L808-L1017)).

**Evidence**: Remove protocol documentation, primary coordinator behavior,
operator decision logic, shared dispatch policy, and persisted CRD shapes.

**Implications**: The kernel-hosted workflow must preserve the same commands,
observations, fences, retry classifications, poison outcomes, irreversible
commit point, and commit-before-cleanup-before-publication ordering.

### Question 3: Which switchover pilot components are genuinely workflow-independent and can be reused unchanged, and which components currently encode switchover-only assumptions that must be generalized?

**Answer**:

- Workflow-independent kernel components are the `Workflow`/`WorkflowContext`
  replay surface, typed bounded activity codec, prepared-activity resolver,
  checkpoint envelope and admission rules, `CheckpointStore` CAS contract,
  fused host methods, and Kubernetes ConfigMap provider
  ([durable-execution/src/workflow.rs:28-128](../../../durable-execution/src/workflow.rs#L28-L128),
  [durable-execution/src/typed.rs:5-181](../../../durable-execution/src/typed.rs#L5-L181),
  [durable-execution/src/store.rs:106-141](../../../durable-execution/src/store.rs#L106-L141),
  [durable-execution/src/host.rs:308-449](../../../durable-execution/src/host.rs#L308-L449),
  [durable-execution/src/host.rs:538-686](../../../durable-execution/src/host.rs#L538-L686)).
- The reusable operator primitives are the exact correlated
  `ReplicaEffectCommand`, UID-fenced `LabelEffectCommand`, portable effect
  outcomes, dispatch-evidence freezing, correlated execution, and dispatch
  failure classification ([kuberic-operator/src/durable/effects.rs:38-146](../../../kuberic-operator/src/durable/effects.rs#L38-L146),
  [kuberic-operator/src/durable/effects.rs:261-446](../../../kuberic-operator/src/durable/effects.rs#L261-L446),
  [kuberic-operator/src/durable/effects.rs:520-559](../../../kuberic-operator/src/durable/effects.rs#L520-L559)).
- Switchover assumptions remain in the feature name and `cfg` boundaries
  ([kuberic-operator/Cargo.toml:11-18](../../../kuberic-operator/Cargo.toml#L11-L18),
  [kuberic-operator/src/durable/mod.rs:15-18](../../../kuberic-operator/src/durable/mod.rs#L15-L18));
  CRD selection/reference names
  ([kuberic-operator/src/crd.rs:43-53](../../../kuberic-operator/src/crd.rs#L43-L53),
  [kuberic-operator/src/crd.rs:118-127](../../../kuberic-operator/src/crd.rs#L118-L127),
  [kuberic-operator/src/crd.rs:184-209](../../../kuberic-operator/src/crd.rs#L184-L209));
  runtime, workflow, activity, terminal, and transcript types
  ([kuberic-operator/src/durable/pilot.rs:120-190](../../../kuberic-operator/src/durable/pilot.rs#L120-L190),
  [kuberic-operator/src/durable/pilot.rs:288-410](../../../kuberic-operator/src/durable/pilot.rs#L288-L410));
  and admission based specifically on `start_switchover`, switchover validation,
  and success/rollback transcripts
  ([kuberic-operator/src/durable/pilot.rs:1380-1549](../../../kuberic-operator/src/durable/pilot.rs#L1380-L1549)).
- The current action validator permits only switchover's seven replica action
  classes, and the adapter rejects explicit decisions outside Execute, label
  patch, Wait, Persist, and Complete
  ([kuberic-operator/src/durable/effects.rs:252-289](../../../kuberic-operator/src/durable/effects.rs#L252-L289),
  [kuberic-operator/src/durable/pilot.rs:898-943](../../../kuberic-operator/src/durable/pilot.rs#L898-L943)).
  Remove-replica additionally emits `RecordCommitEvidence`, `DeletePod`, and
  `CommitSnapshot` decisions
  ([kuberic-operator/src/durable/remove_replica.rs:766-802](../../../kuberic-operator/src/durable/remove_replica.rs#L766-L802),
  [kuberic-operator/src/durable/remove_replica.rs:954-1007](../../../kuberic-operator/src/durable/remove_replica.rs#L954-L1007)).
- The measured pilot store is not workflow-neutral as written: it imports
  `PilotActivityKind`, classifies only passive/replica/label activities, and
  emits switchover-named telemetry
  ([kuberic-operator/src/durable/pilot_store.rs:13-16](../../../kuberic-operator/src/durable/pilot_store.rs#L13-L16),
  [kuberic-operator/src/durable/pilot_store.rs:164-205](../../../kuberic-operator/src/durable/pilot_store.rs#L164-L205)).
  The reconciliation loop and condition type are also bound to Switchover and
  `durable_switchover_pilot`
  ([kuberic-operator/src/reconciler.rs:2633-2672](../../../kuberic-operator/src/reconciler.rs#L2633-L2672),
  [kuberic-operator/src/reconciler.rs:3007-3042](../../../kuberic-operator/src/reconciler.rs#L3007-L3042)).

**Evidence**: Kernel public contracts, operator effect adapter, pilot workflow,
store instrumentation, CRD schema, and reconciler integration.

**Implications**: The spec can require reuse of the kernel and generic effect
primitives unchanged, but must explicitly cover generalization of
switchover-named hosting/status/telemetry and support for remove-specific
decision/effect kinds.

### Question 4: How do prepared effect exposure, typed activities, fused progression, checkpoint admission, terminal compaction, and the ConfigMap checkpoint store integrate in the existing switchover pilot?

**Answer**:

1. Acceptance first writes a random execution ID, deterministic checkpoint
   name, and exact initial explicit switchover operation into status; checkpoint
   creation occurs on a later reconcile
   ([kuberic-operator/src/durable/pilot.rs:1380-1412](../../../kuberic-operator/src/durable/pilot.rs#L1380-L1412),
   [kuberic-operator/src/reconciler.rs:1365-1401](../../../kuberic-operator/src/reconciler.rs#L1365-L1401)).
2. The ordinary-async workflow calls one typed, versioned, bounded activity
   contract; deterministic explicit `Persist` transitions are folded in memory
   before another durable activity is requested
   ([kuberic-operator/src/durable/pilot.rs:332-342](../../../kuberic-operator/src/durable/pilot.rs#L332-L342),
   [kuberic-operator/src/durable/pilot.rs:412-616](../../../kuberic-operator/src/durable/pilot.rs#L412-L616)).
3. `PilotPreparedActivityResolver` converts the logical passive request into
   the exact replica or label command derived from current authoritative
   observations. Replay accepts only the exact recorded prepared specification
   ([kuberic-operator/src/durable/pilot.rs:962-1137](../../../kuberic-operator/src/durable/pilot.rs#L962-L1137)).
4. `turn_and_expose_with` persists that prepared command directly in
   `DispatchExposed` state, reserves its maximum result size, and returns a
   private dispatch permit only after CAS acceptance
   ([durable-execution/src/host.rs:308-449](../../../durable-execution/src/host.rs#L308-L449),
   [durable-execution/src/host.rs:711-761](../../../durable-execution/src/host.rs#L711-L761)).
   The bridge consumes the matching permit once before dispatch
   ([kuberic-operator/src/durable/pilot.rs:1149-1189](../../../kuberic-operator/src/durable/pilot.rs#L1149-L1189),
   [kuberic-operator/src/durable/effects.rs:597-655](../../../kuberic-operator/src/durable/effects.rs#L597-L655)).
5. `observe_and_turn_with` atomically persists an authoritative result together
   with either the next prepared exposure or the terminal checkpoint. Conflict
   or unknown CAS results request reload; an already exposed activity reloads
   as quarantined and cannot receive another permit
   ([durable-execution/src/host.rs:538-686](../../../durable-execution/src/host.rs#L538-L686),
   [kuberic-operator/src/reconciler.rs:2725-2913](../../../kuberic-operator/src/reconciler.rs#L2725-L2913)).
6. Admission limits the pilot to three replicas, 32 activity records, 64
   deterministic transitions, 3,000-byte operation input, 8,192-byte activity
   input, 4,096-byte activity result/terminal payload, and a 752-KiB encoded
   checkpoint ([kuberic-operator/src/durable/pilot.rs:48-58](../../../kuberic-operator/src/durable/pilot.rs#L48-L58),
   [kuberic-operator/src/durable/pilot.rs:1483-1549](../../../kuberic-operator/src/durable/pilot.rs#L1483-L1549)).
   The kernel separately proves terminal capacity and maximum activity-result
   capacity before exposure
   ([durable-execution/src/checkpoint.rs:276-370](../../../durable-execution/src/checkpoint.rs#L276-L370),
   [durable-execution/src/host.rs:711-731](../../../durable-execution/src/host.rs#L711-L731)).
7. Completion replaces active history with an immutable terminal outcome and
   completed-activity count; terminal reload bypasses workflow polling
   ([durable-execution/src/checkpoint.rs:160-239](../../../durable-execution/src/checkpoint.rs#L160-L239),
   [durable-execution/src/host.rs:340-354](../../../durable-execution/src/host.rs#L340-L354)).
   The reconciler validates and observes this terminal checkpoint before
   publishing final topology/status
   ([kuberic-operator/src/reconciler.rs:2649-2661](../../../kuberic-operator/src/reconciler.rs#L2649-L2661),
   [kuberic-operator/src/reconciler.rs:2891-2908](../../../kuberic-operator/src/reconciler.rs#L2891-L2908)).
8. The Kubernetes provider stores one execution per same-namespace ConfigMap
   named `kuberic-checkpoint-<execution-id>`, uses opaque
   `resourceVersion` compare-and-swap, enforces a data budget, and validates the
   non-controlling owner relationship
   ([durable-execution/src/kubernetes.rs:195-253](../../../durable-execution/src/kubernetes.rs#L195-L253),
   [durable-execution/src/kubernetes.rs:283-385](../../../durable-execution/src/kubernetes.rs#L283-L385),
   [kuberic-operator/src/durable/pilot.rs:1452-1481](../../../kuberic-operator/src/durable/pilot.rs#L1452-L1481)).

**Evidence**: Pilot workflow/resolver/runtime, fused host operations, checkpoint
validation, and Kubernetes provider behavior.

**Implications**: Remove-replica must enter through the same accepted-reference
ordering and retain prepared-command identity, result reservation, no-permit-on-
uncertainty behavior, bounded admission, terminal-before-status publication,
and owner-bound ConfigMap CAS semantics.

### Question 5: What feature-gating and reconciler-test patterns establish that the switchover pilot is default-off and leaves explicit behavior unchanged?

**Answer**:

- The operator feature set is empty by default; the pilot and its kernel/Kube
  dependencies are enabled only by `durable-switchover-pilot`
  ([kuberic-operator/Cargo.toml:11-18](../../../kuberic-operator/Cargo.toml#L11-L18)).
  Pilot modules, runtime construction, and reconciliation are compile-time
  gated ([kuberic-operator/src/durable/mod.rs:15-18](../../../kuberic-operator/src/durable/mod.rs#L15-L18),
  [kuberic-operator/src/main.rs:34-37](../../../kuberic-operator/src/main.rs#L34-L37),
  [kuberic-operator/src/reconciler.rs:1613-1634](../../../kuberic-operator/src/reconciler.rs#L1613-L1634)).
- The CRD field defaults to `Explicit`; `DurablePilot` is a separate value
  ([kuberic-operator/src/crd.rs:184-191](../../../kuberic-operator/src/crd.rs#L184-L191)).
  Requesting it without the build feature returns an error rather than silently
  falling back ([kuberic-operator/src/reconciler.rs:187-204](../../../kuberic-operator/src/reconciler.rs#L187-L204)).
- Unit tests assert that explicit mode is always available and pilot mode
  never silently falls back
  ([kuberic-operator/src/reconciler.rs:4218-4241](../../../kuberic-operator/src/reconciler.rs#L4218-L4241));
  CRD tests assert omission defaults to explicit
  ([kuberic-operator/src/crd.rs:1007-1020](../../../kuberic-operator/src/crd.rs#L1007-L1020)).
- The normal reconciler integration test drives the default explicit
  switchover and verifies its mutation and data-plane behavior
  ([examples/kvstore/tests/reconciler.rs:3563-3764](../../../examples/kvstore/tests/reconciler.rs#L3563-L3764)).
  The pilot happy-path test separately opts in and then verifies a subsequent
  normal set starts an explicit operation and clears the old pilot reference
  ([examples/kvstore/tests/reconciler.rs:3766-4008](../../../examples/kvstore/tests/reconciler.rs#L3766-L4008)).
- CI runs default all-target builds plus explicit feature-enabled pilot
  measurement tests, keeping both configurations exercised
  ([.github/workflows/CI.yml:47-64](../../../.github/workflows/CI.yml#L47-L64)).

**Evidence**: Cargo features, `cfg` sites, CRD defaults, unit tests, integration
tests, and CI commands.

**Implications**: Durable remove-replica needs its own explicit default-off
feature and opt-in selection, with default explicit regression coverage and a
no-silent-fallback test matching this pattern.

### Question 6: What exact non-overlapping source scopes does the current complexity script charge to the switchover workflow body, comparable legacy scope, shared reusable infrastructure, and operator integration?

**Answer**:

- The script defines the measurement groups and source files at
  [scripts/measure-switchover-complexity.py:16-50](../../../scripts/measure-switchover-complexity.py#L16-L50).
  Marker locations are:
  - pilot module: `pilot.rs` lines 48-2080;
  - nested comparable workflow scope: `pilot.rs` lines 410-1377;
  - nested workflow body: `pilot.rs` lines 412-615;
  - shared effect adapters: `effects.rs` lines 38-848;
  - measured store: `pilot_store.rs` lines 19-381;
  - effect bridge: `reconciler.rs` lines 373-455;
  - pilot reconcile: `reconciler.rs` lines 2634-3272;
  - fused kernel turn: `host.rs` lines 309-448;
  - fused kernel observe: `host.rs` lines 539-685;
  - shared typed kernel: the complete `durable-execution/src/typed.rs`.
  The markers themselves are visible at
  [kuberic-operator/src/durable/pilot.rs:47-48](../../../kuberic-operator/src/durable/pilot.rs#L47-L48),
  [kuberic-operator/src/durable/pilot.rs:409-412](../../../kuberic-operator/src/durable/pilot.rs#L409-L412),
  [kuberic-operator/src/durable/pilot.rs:616-616](../../../kuberic-operator/src/durable/pilot.rs#L616),
  [kuberic-operator/src/durable/pilot.rs:1378-1378](../../../kuberic-operator/src/durable/pilot.rs#L1378),
  [kuberic-operator/src/durable/pilot.rs:2081-2081](../../../kuberic-operator/src/durable/pilot.rs#L2081),
  [kuberic-operator/src/durable/effects.rs:37-849](../../../kuberic-operator/src/durable/effects.rs#L37-L849),
  [kuberic-operator/src/durable/pilot_store.rs:18-382](../../../kuberic-operator/src/durable/pilot_store.rs#L18-L382),
  [kuberic-operator/src/reconciler.rs:372-456](../../../kuberic-operator/src/reconciler.rs#L372-L456),
  [kuberic-operator/src/reconciler.rs:2633-3273](../../../kuberic-operator/src/reconciler.rs#L2633-L3273),
  and [durable-execution/src/host.rs:308-686](../../../durable-execution/src/host.rs#L308-L686).
- `workflow_body_only` and `workflow_comparable_legacy_scope` are nested reports,
  not additive charges. `shared_reusable_infrastructure` is effect adapters +
  all typed-kernel code + the two fused-host ranges. `operator_integration` is
  measured store + effect bridge + reconcile. The total adds pilot module +
  shared + integration exactly once, and the script rejects overlaps among
  those charged groups
  ([scripts/measure-switchover-complexity.py:70-100](../../../scripts/measure-switchover-complexity.py#L70-L100),
  [scripts/measure-switchover-complexity.py:112-181](../../../scripts/measure-switchover-complexity.py#L112-L181)).
- Running the script at this commit reports: workflow body 201/29,
  comparable scope 930/108, shared reusable infrastructure 1,208/110, operator
  integration 1,047/55, pilot non-overlapping total 4,161/326, explicit
  switchover 1,449/172, and combined total 5,610/498. These are
  executable-lines/decision-points and match the currently documented values
  ([docs/features/kuberic/durable-execution-roadmap.md:205-210](../../../docs/features/kuberic/durable-execution-roadmap.md#L205-L210)).

**Evidence**: Complexity-script declarations, boundary markers, overlap
validation, aggregation logic, and an execution of the script at the recorded
commit.

**Implications**: Remove-replica measurement must preserve nested reporting,
define new disjoint charged scopes, avoid double-counting generalized shared
code, retain an explicit comparable baseline, and report both marginal and
combined totals.

### Question 7: Which existing tests define the remove-replica lifecycle regression matrix and which pilot tests provide patterns for replay, effect, checkpoint, and reconciler coverage?

**Answer**:

- Remove decision-unit coverage in
  `kuberic-operator/src/durable/remove_replica.rs` includes start/admission and
  v1 rejection, deterministic coarse intent, exact commit recording,
  compensation, post-dispatch ambiguity, generation drift, bounded redrive,
  resource-version conflict replay, UID-fenced cleanup, connection-absence
  proof, retirement expiry, typed poison dispositions, and restart no-op
  behavior
  ([kuberic-operator/src/durable/remove_replica.rs:1870-3027](../../../kuberic-operator/src/durable/remove_replica.rs#L1870-L3027)).
- End-to-end reconciler coverage includes coarse activation, unreachable
  `Force`, malformed status, intent/status conflict and UID fencing,
  scale-down pre-admission/minimum, same-UID generation drift, no ScaleDown→Force
  conversion, commit/publication conflicts, and full scale-down
  ([examples/kvstore/tests/reconciler.rs:6343-6909](../../../examples/kvstore/tests/reconciler.rs#L6343-L6909),
  [examples/kvstore/tests/reconciler.rs:7914-8011](../../../examples/kvstore/tests/reconciler.rs#L7914-L8011),
  [examples/kvstore/tests/reconciler.rs:8501-8565](../../../examples/kvstore/tests/reconciler.rs#L8501-L8565)).
- Core coordinator/peer tests cover exact admission, invalid progress,
  lost replies, phase-postcondition resume, compensation/expiry, stale-primary
  authority, responsiveness during held work, post-deadline roll-forward,
  all clock budgets, role-none-before-close ordering, exact peer fencing,
  replacement protection, restart recovery, and bounded terminal retention
  ([kuberic-core/src/replica_agent.rs:5539-7358](../../../kuberic-core/src/replica_agent.rs#L5539-L7358),
  [kuberic-core/src/replica_lifecycle.rs:628-739](../../../kuberic-core/src/replica_lifecycle.rs#L628-L739)).
- Pilot workflow/replay patterns are the pilot unit tests for terminal reload
  without polling, restart quarantine, permit identity, prepared-command fence
  binding, semantic replay mismatch, bounded redelivery, terminal accounting,
  schedule/observation/exposure faults, and admission projections
  ([kuberic-operator/src/durable/pilot.rs:2120-4008](../../../kuberic-operator/src/durable/pilot.rs#L2120-L4008)).
  Generic typed/prepared replay and mismatch coverage is in
  [durable-execution/tests/replay.rs:104-1107](../../../durable-execution/tests/replay.rs#L104-L1107).
- Effect-adapter patterns verify distinct portable outcomes, unknown versus
  proven non-admission, all seven switchover replica commands, and preparation
  rejection on identity/protocol drift
  ([kuberic-operator/src/durable/effects.rs:971-1177](../../../kuberic-operator/src/durable/effects.rs#L971-L1177)).
- Checkpoint-store patterns verify authoritative versus unknown byte
  accounting, terminal-only accounting recovery, bounded event history, exact
  opaque revisions, ownership, conflicts, conservative mutation
  classification, data budgets, and unsupported-format reload behavior
  ([kuberic-operator/src/durable/pilot_store.rs:520-840](../../../kuberic-operator/src/durable/pilot_store.rs#L520-L840),
  [durable-execution/tests/kubernetes_checkpoint.rs:189-936](../../../durable-execution/tests/kubernetes_checkpoint.rs#L189-L936)).
- Reconciler pilot coverage includes happy path and later explicit selection,
  proven-no-admission redelivery, every-turn restart, compensation, lost reply
  windows, failed status publication followed by terminal reload, terminal CAS
  conflict, stale incarnation, unknown checkpoint outcomes, and distinct
  repeated execution identity
  ([examples/kvstore/tests/reconciler.rs:3766-4898](../../../examples/kvstore/tests/reconciler.rs#L3766-L4898)).

**Evidence**: Existing unit, integration, kernel replay, provider, and
reconciler test matrices.

**Implications**: Durable remove-replica coverage should pair the complete
existing removal lifecycle matrix with pilot-style replay, preparation,
exposure, quarantine, checkpoint fault, terminal reload, restart, and
default-explicit tests.

### Question 8: How is the feasibility classifier derived, and how can its runtime-neutrality predicate inspect only library dependencies while still rejecting a real runtime dependency?

**Answer**:

- The classifier is mechanical: failed safety/determinism is `Infeasible`;
  otherwise any conformance failure, authoring-simplicity failure, or in-scope
  limitation is `ConditionallyFeasible`; only all-pass/no-limitation is
  `Feasible`
  ([durable-execution/src/assessment.rs:1-29](../../../durable-execution/src/assessment.rs#L1-L29)).
  The feasibility test derives the four inputs from the 45-scenario registry,
  FR-012 predicates, and five revision predicates
  ([durable-execution/tests/feasibility.rs:44-205](../../../durable-execution/tests/feasibility.rs#L44-L205),
  [durable-execution/tests/feasibility.rs:269-302](../../../durable-execution/tests/feasibility.rs#L269-L302)).
- The runtime-neutrality predicate currently checks async API text and then
  applies `!crate_manifest.contains("tokio")` to the entire `Cargo.toml`
  ([durable-execution/tests/feasibility.rs:174-181](../../../durable-execution/tests/feasibility.rs#L174-L181)).
  Because `tokio.workspace = true` exists only under `[dev-dependencies]`,
  the predicate currently fails even though the library dependency table has no
  runtime dependency
  ([durable-execution/Cargo.toml:16-29](../../../durable-execution/Cargo.toml#L16-L29)).
  Running the targeted evidence test at this commit reports that predicate as
  failed, `has_in_scope_limitation=true`, and classification
  `conditionally feasible`.
- To represent the stated contract, dependency inspection must be scoped to
  the library dependency table (`[dependencies]`) rather than substring
  matching the full manifest. The regression evidence must also include a
  manifest fixture with a real runtime dependency in `[dependencies]` and
  assert rejection, while allowing the current `tokio` dev-dependency.

**Evidence**: Classifier truth table, input derivation, current source-text
predicate, manifest section boundaries, and targeted feasibility-test output.

**Implications**: The specification should require table-aware
runtime-dependency classification plus positive and negative tests; after that
correction, the classification must be recomputed from the same evidence
rather than hard-coded.

### Question 9: Which README and roadmap statements are stale after PR #48, and what current evidence or contracts should replace them?

**Answer**:

- `durable-execution/README.md` says all five revision predicates pass and the
  result is conditionally feasible because of a write-efficiency exception
  ([durable-execution/README.md:368-375](../../../durable-execution/README.md#L368-L375)).
  Current executable evidence instead has one failed runtime-neutrality
  predicate caused by the full-manifest `tokio` scan, while PR #48's prepared
  exposure removed the seven preparation-only records and achieved 13 accepted
  writes for 12 durable boundaries
  ([durable-execution/tests/feasibility.rs:174-189](../../../durable-execution/tests/feasibility.rs#L174-L189),
  [docs/features/kuberic/durable-execution-roadmap.md:168-176](../../../docs/features/kuberic/durable-execution-roadmap.md#L168-L176)).
  The README should describe the classifier result produced after the predicate
  is corrected, with the exact current predicate outcomes, rather than retain
  the removed write-efficiency rationale.
- The README's scope statement says the feature-gated pilot does not change
  remove-replica or any other topology workflow
  ([durable-execution/README.md:401-408](../../../durable-execution/README.md#L401-L408)).
  That is accurate for the present commit but becomes stale when this authorized
  second kernel-hosted workflow lands; the replacement contract should state
  exactly which workflows have opt-in kernel paths and that explicit paths
  remain the defaults.
- The roadmap says shared code may amortize only across later workflows but
  claims no amortization yet
  ([docs/features/kuberic/durable-execution-roadmap.md:205-211](../../../docs/features/kuberic/durable-execution-roadmap.md#L205-L211)),
  says no other workflow is authorized
  ([docs/features/kuberic/durable-execution-roadmap.md:213-220](../../../docs/features/kuberic/durable-execution-roadmap.md#L213-L220)),
  and lists adoption by existing Kuberic workflows as explicitly deferred
  ([docs/features/kuberic/durable-execution-roadmap.md:225-232](../../../docs/features/kuberic/durable-execution-roadmap.md#L225-L232)).
  The work-item authorization and the resulting measured second workflow must
  replace those claims with per-workflow marginal scopes, actually shared
  reusable scope, operator integration, combined totals, and the retained
  default-off/no-broad-migration boundary.
- Current stable evidence that should remain is the prepared-exposure contract,
  9 external effects + 3 passive observations + 13 accepted writes for the
  no-redelivery switchover path, 752-KiB checkpoint admission, 4,096-byte
  terminal payload bound, and the script's overlap-checked 201/29, 930/108,
  1,208/110, 1,047/55, 4,161/326, and 5,610/498 lexical measurements
  ([docs/features/kuberic/durable-execution-roadmap.md:168-214](../../../docs/features/kuberic/durable-execution-roadmap.md#L168-L214)).
  Run-specific observed checkpoint byte ranges are measurements, not
  compatibility contracts
  ([docs/features/kuberic/durable-execution-roadmap.md:176-184](../../../docs/features/kuberic/durable-execution-roadmap.md#L176-L184)).

**Evidence**: Current README claims, roadmap scope/measurement text, feasibility
predicate, and PR #48's prepared-exposure behavior as represented in current
source and executable measurements.

**Implications**: Documentation updates must separate stable contracts from
run-specific measurements, report the corrected mechanical feasibility result,
and acknowledge remove-replica as the second explicitly gated kernel-hosted
workflow without implying broad migration.

## Open Unknowns

None. All internal questions were answerable from the current repository and
targeted local evidence.

## User-Provided External Knowledge (Manual Fill)

None provided.
