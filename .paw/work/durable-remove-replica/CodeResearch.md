---
date: 2026-09-07 04:13:49 UTC
git_commit: 740ee5119327d21ab5d7987bb1fab0df522096be
branch: feature/durable-remove-replica
repository: kuberic
topic: "Durable Remove Replica Code Research"
tags: [research, codebase, remove-replica, durable-execution, operator, complexity]
status: complete
last_updated: 2026-09-07
---

# Research: Durable Remove Replica

## Research Question

Map the current implementation terrain for porting remove-replica onto the
durable-execution kernel as a separately gated second kernel-hosted workflow.
The map covers the explicit removal control/data flow and regression tests,
switchover host boundaries, feature/CRD/reconciler routing, effect and
checkpoint extension points, complexity attribution, feasibility evidence,
test-fixture conventions, documentation conventions, and validation commands.

The behavioral requirements and terminology come from
`.paw/work/durable-remove-replica/Spec.md:1-499`; the prior behavioral research
is recorded in `.paw/work/durable-remove-replica/SpecResearch.md:1-531`; and
the workflow binding fixes the branch, single-model policy, and work-item
identity in `.paw/work/durable-remove-replica/WorkflowContext.md:1-24`.

## Summary

- The current remove-replica path is an explicit, CRD-status-backed operation.
  Healthy scale-down and several stale/missing-secondary recovery branches call
  `start_remove_replica`, persist `Phase::RemovingReplica`, and resume through
  the shared explicit-operation reconciler
  (`kuberic-operator/src/reconciler.rs:771-816`,
  `kuberic-operator/src/reconciler.rs:1028-1079`,
  `kuberic-operator/src/reconciler.rs:1280-1329`,
  `kuberic-operator/src/reconciler.rs:1477-1518`,
  `kuberic-operator/src/reconciler.rs:1646-1656`).
- The explicit decision engine persists a version-2 frozen operation, issues
  one correlated `RemoveReplicaIntent` to the exact primary, records reduced
  Current commit evidence before cleanup, then performs UID-fenced retirement
  labeling, deletion, stable-snapshot publication, and completion
  (`kuberic-operator/src/durable/remove_replica.rs:51-146`,
  `kuberic-operator/src/durable/remove_replica.rs:152-229`,
  `kuberic-operator/src/durable/remove_replica.rs:374-802`,
  `kuberic-operator/src/durable/remove_replica.rs:805-1017`).
- The durable-execution crate already provides workflow-neutral replay,
  typed/bounded activities, prepared-activity resolution, exact checkpoint
  validation, CAS storage, prepared exposure, result reservation, fused
  observation/progression, quarantine, and terminal compaction
  (`durable-execution/src/workflow.rs:24-128`,
  `durable-execution/src/typed.rs:5-181`,
  `durable-execution/src/checkpoint.rs:10-370`,
  `durable-execution/src/store.rs:21-141`,
  `durable-execution/src/host.rs:10-165`,
  `durable-execution/src/host.rs:308-449`,
  `durable-execution/src/host.rs:538-686`).
- The existing operator host is switchover-specific above those kernel
  contracts: its Cargo feature, CRD selection/reference, runtime/cache,
  workflow state/activity/terminal types, action whitelist, measurement
  decoder, condition name, and reconciliation loop all use pilot/switchover
  types or names (`kuberic-operator/Cargo.toml:11-18`,
  `kuberic-operator/src/crd.rs:43-55`,
  `kuberic-operator/src/crd.rs:118-127`,
  `kuberic-operator/src/crd.rs:184-209`,
  `kuberic-operator/src/durable/pilot.rs:47-170`,
  `kuberic-operator/src/durable/pilot.rs:238-410`,
  `kuberic-operator/src/reconciler.rs:2633-3273`).
- The current complexity script has explicit marker-based, overlap-checked
  charged scopes. It reports nested diagnostic workflow scopes separately and
  adds only the pilot module, shared scope, and integration scope to its
  non-overlapping total (`scripts/measure-switchover-complexity.py:13-50`,
  `scripts/measure-switchover-complexity.py:53-100`,
  `scripts/measure-switchover-complexity.py:112-181`).
- The runtime-neutrality feasibility predicate currently scans the complete
  durable-execution manifest for `tokio`, although `tokio` appears only under
  `[dev-dependencies]`; the current executable classification is therefore
  `conditionally feasible` with that single revision predicate failing
  (`durable-execution/tests/feasibility.rs:174-205`,
  `durable-execution/tests/feasibility.rs:269-302`,
  `durable-execution/Cargo.toml:10-29`).

## Documentation System

- **Framework**: Plain Markdown committed with the source tree. No MkDocs,
  Docusaurus, Sphinx, mdBook, sidebar, or navigation configuration is present.
- **Docs Directory**: `docs/`, with feature documents under
  `docs/features/`, implemented ADR-like documents under
  `docs/features/kuberic/implemented/`, and background studies under
  `docs/background/` (`README.md:67-72`).
- **Navigation Config**: None. The root README provides a manually maintained
  design-document link list (`README.md:65-72`).
- **Style Conventions**:
  - One H1 title followed by H2/H3 sections
    (`durable-execution/README.md:1-8`,
    `docs/features/kuberic/durable-execution-roadmap.md:1-13`,
    `docs/features/kuberic/testing.md:1-10`).
  - Relative Markdown links connect repository documents
    (`README.md:67-72`,
    `durable-execution/README.md:377-383`).
  - Commands use fenced `bash` or `console` blocks
    (`README.md:31-40`,
    `docs/features/kuberic/testing.md:200-220`,
    `docs/features/kuberic/testing.md:452-493`).
  - Design and testing documents use numbered lists, bullet inventories,
    tables, bold labels, and explicit status language
    (`docs/features/kuberic/testing.md:187-220`,
    `docs/features/kuberic/testing.md:276-318`,
    `docs/features/kuberic/implemented/agent-owned-replica-remove-protocol.md:1-48`).
  - Measurement prose distinguishes stable limits from run-specific samples
    and effect/observation counts from checkpoint writes
    (`docs/features/kuberic/durable-execution-roadmap.md:168-205`).
- **Build Command**: N/A; no documentation generator or docs build is
  configured. Rust documentation tests use `cargo test --doc --workspace`
  (`docs/features/kuberic/testing.md:200-210`).
- **Standard Files**: `README.md`, crate READMEs at
  `durable-execution/README.md`, `kuberic-core/README.md`, and
  `kuberic-operator/README.md`; no `CHANGELOG` or `CONTRIBUTING` file is
  present.

## Verification Commands

- **Format**: `cargo fmt --all -- --check`
  (`.github/workflows/CI.yml:46-48`).
- **Check**: `cargo check --all-targets`
  (`.github/workflows/CI.yml:43-44`).
- **Lint**: `cargo clippy --all-targets -- -D warnings`
  (`.github/workflows/CI.yml:52-53`). The specification additionally names
  all-feature warning-free linting as required coverage
  (`.paw/work/durable-remove-replica/Spec.md:330-356`).
- **Build**: `cargo build --all-targets`
  (`.github/workflows/CI.yml:64-65`).
- **Full Test**: `cargo test --all --all-features`
  (`.github/workflows/CI.yml:77-78`).
- **Meaningful non-cluster suites**:
  `cargo test -p kuberic-core -p kuberic-operator -p kvstore -p sqlite-replicated`
  (`docs/features/kuberic/testing.md:200-205`).
- **Explicit remove integration test example**:
  `cargo test -p kvstore --test reconciler test_durable_remove_coarse_activation`
  (`docs/features/kuberic/testing.md:212-219`).
- **Durable kernel default suite**:
  `cargo test -p kuberic-durable-execution`; Kubernetes-provider suite:
  `cargo test -p kuberic-durable-execution --features kubernetes`
  (crate package/features at `durable-execution/Cargo.toml:1-14`; the real API
  command is documented at `docs/features/kuberic/testing.md:479-493`).
- **Current switchover feature gates**:
  `cargo test -p kuberic-operator --features durable-switchover-pilot
  success_and_rollback_transcripts_fit_with_redelivery_headroom`,
  `cargo test -p kuberic-operator --features durable-switchover-pilot
  maximum_projected_history_fits_both_budgets`, and
  `cargo test -p kuberic-operator --features durable-switchover-pilot
  measurements_ -- --nocapture`
  (`.github/workflows/CI.yml:55-62`,
  `docs/features/kuberic/testing.md:452-463`).
- **Current pilot reconciler matrix**:
  `CARGO_BUILD_JOBS=2 CARGO_INCREMENTAL=0 RUST_MIN_STACK=4194304 cargo test
  -p kvstore --test reconciler test_durable_execution_switchover_pilot_`
  (`docs/features/kuberic/testing.md:474-482`).
- **Feasibility evidence**:
  `cargo test -p kuberic-durable-execution --test feasibility
  mechanically_assesses_the_selected_surface_and_full_denominator -- --nocapture`
  (test entry at `durable-execution/tests/feasibility.rs:44-302`).
- **Complexity evidence**:
  `python3 scripts/measure-switchover-complexity.py`
  (`.github/workflows/CI.yml:49-50`,
  `docs/features/kuberic/testing.md:405-410`).
- **Container/cluster recipes**: `just images` is the CI image-build entry;
  `justfile:1-44` defines KinD creation, image build/load, and deployment
  recipes (`.github/workflows/CI.yml:67-75`).

There is no separate static type-check command; Rust type checking is covered
by `cargo check --all-targets`.

## Detailed Findings

### 1. Explicit Remove-Replica Selection and Initialization

#### Selection routes

The reconciler starts removal from four observable classes:

1. A ready stable secondary whose runtime restarted under the same Pod UID but
   no longer matches its committed role/epoch is started in `Force` mode
   (`kuberic-operator/src/reconciler.rs:771-816`).
2. A committed stable secondary with no current Pod is started in `Force`
   mode (`kuberic-operator/src/reconciler.rs:1028-1079`).
3. A stale old incarnation whose replacement Pod is not ready is removed in
   `Force` mode while preserving the historical UID and replicator address
   (`kuberic-operator/src/reconciler.rs:1280-1329`).
4. Healthy scale-down chooses the highest-ID non-primary stable member,
   preadmits its exact live generation, and starts `ScaleDown`
   (`kuberic-operator/src/reconciler.rs:1477-1518`).

Each route constructs `RemoveReplicaTarget` with replica ID, Pod name, exact
historical UID/incarnation, historical replicator address, and either a
preadmitted generation (`ScaleDown`) or no generation (`Force`)
(`kuberic-operator/src/durable/remove_replica.rs:36-43`,
`kuberic-operator/src/reconciler.rs:788-807`,
`kuberic-operator/src/reconciler.rs:1488-1507`).

#### Initial persisted operation

`start_remove_replica`:

- validates the previous stable snapshot and target fields;
- rejects the primary, non-members, non-active-secondaries, UID/incarnation
  mismatch, membership below `minReplicas`, and retained membership below the
  previous write quorum;
- removes the target from a cloned snapshot and recomputes the target write
  quorum;
- creates a deterministic operation identity containing operation version 2,
  epoch, target ID, and target incarnation; and
- returns phase `RemoveFreezeIntent` with previous and reduced snapshots,
  mode, target identities, historical address, optional preadmitted
  generation, and the first overall deadline
  (`kuberic-operator/src/durable/remove_replica.rs:51-146`).

The CRD status shape stores the complete explicit operation in
`KubericSetStatus.operation`; the remove-specific fields include mode, target
identity, intent, commit evidence, cleanup, and typed disposition
(`kuberic-operator/src/crd.rs:109-127`,
`kuberic-operator/src/crd.rs:296-358`,
`kuberic-operator/src/crd.rs:380-499`).

### 2. Explicit Remove-Replica Decision and Persistence Flow

`decide_remove_replica` validates the persisted operation on every invocation,
returns `Wait` for terminal phases, routes any operation with durable commit
evidence directly to post-commit cleanup, and otherwise dispatches by
pre-commit phase (`kuberic-operator/src/durable/remove_replica.rs:152-229`).

#### Freeze and correlated dispatch

- `RemoveFreezeIntent` observes the exact writable primary and, for
  `ScaleDown`, the exact preadmitted target generation and lifecycle-peer
  protocol. It derives previous/reduced configuration descriptors, assigns an
  attempt/attempt ID/action ID, freezes primary generation/control version,
  endpoints, target peer authority, quorum/minimum values, and all deadlines,
  validates the core intent, and persists `RemoveDispatchIntent`
  (`kuberic-operator/src/durable/remove_replica.rs:232-371`).
- `RemoveDispatchIntent` persists a `PendingActionStatus` for
  `RemoveReplicaIntent`, targeting the old primary with the exact primary
  incarnation and `RemoveReplicaCoordinated` postcondition, then advances to
  `RemoveAwaitCoordination`
  (`kuberic-operator/src/durable/remove_replica.rs:173-204`).
- The shared explicit reconciler first persists dispatch evidence. For coarse
  add/remove intents it keeps the structured action authoritative rather than
  serializing a direct payload, then calls
  `execute_planned_control_action`
  (`kuberic-operator/src/reconciler.rs:3487-3576`).
- `execute_planned_control_action` reconstructs the correlated request from
  the exact persisted generation, control version, runtime epoch, action ID,
  target identity, and authoritative structured action
  (`kuberic-operator/src/durable/effects.rs:367-413`).

#### Observation, retry, commit, and poison

- Await coordination reads current or retained correlated action evidence from
  the exact primary, checks the frozen signature, validates typed progress,
  persists newly observed phase/result/expiry/current-install evidence, and
  interprets live configuration together with action state
  (`kuberic-operator/src/durable/remove_replica.rs:374-492`,
  `kuberic-operator/src/durable/remove_replica.rs:559-718`).
- A new primary-agent generation is classified using exact live configuration
  and Pod identity. Exact previous Current or reduced CatchUp can return to
  `RemoveFreezeIntent` within the three-attempt bound; exact reduced Current
  records commit; a complete same-Pod restart without surviving evidence is a
  typed ambiguous restart
  (`kuberic-operator/src/durable/remove_replica.rs:494-557`).
- Dispatch failure policy is shared with other explicit operations:
  precondition/continuity rejection clears dispatch evidence, `AgentBusy`
  retains it for retry, conflict poisons, and other errors are recorded without
  asserting non-admission
  (`kuberic-operator/src/durable/effects.rs:530-589`).
- Commit recording persists phase `RemoveRecordCommit`, the exact reduced
  workflow-scoped `committed_snapshot`, frozen action/generation/configuration
  evidence, and initial cleanup facts. It returns
  `Decision::RecordCommitEvidence`, which the reconciler writes to CRD status
  without publishing `stable_snapshot`
  (`kuberic-operator/src/durable/remove_replica.rs:720-802`,
  `kuberic-operator/src/reconciler.rs:3686-3700`).
- Typed poison dispositions are `FailedPreCommitIncomplete`,
  `InvalidRemovalState`, and `AmbiguousPrimaryRestart`
  (`kuberic-operator/src/crd.rs:476-499`,
  `kuberic-operator/src/durable/remove_replica.rs:1518-1565`).

#### Post-commit cleanup and publication

- `RemoveRecordCommit` first persists `RemoveAwaitCleanup`
  (`kuberic-operator/src/durable/remove_replica.rs:818-822`).
- Cleanup requires exact reduced Current from the committed primary
  generation, merges connection and retirement evidence from correlated
  progress, and treats primary process absence/replacement separately from a
  missing status response from the still-present exact process
  (`kuberic-operator/src/durable/remove_replica.rs:823-912`).
- After connection absence, terminal retirement evidence is either observed or
  frozen as unavailable at retirement expiry. The old target is then
  exact-UID patched to `role=retired`; a replacement UID is treated as already
  fenced (`kuberic-operator/src/durable/remove_replica.rs:913-955`).
- `RemoveDeleteTargetPod` issues exact-UID deletion while the old UID exists,
  then persists deletion evidence and advances to publication
  (`kuberic-operator/src/durable/remove_replica.rs:956-973`).
- `RemovePublishTopology` verifies all cleanup facts and returns
  `CommitSnapshot`; the reconciler publishes the reduced stable snapshot and
  operation phase before `RemoveFinalize` returns `Complete`
  (`kuberic-operator/src/durable/remove_replica.rs:974-1017`,
  `kuberic-operator/src/reconciler.rs:3647-3685`,
  `kuberic-operator/src/reconciler.rs:3702-3747`).
- While removal has commit evidence but has not yet published the reduced
  stable snapshot, `authoritative_topology_snapshot` uses
  `committed_snapshot`; otherwise it uses `stable_snapshot`
  (`kuberic-operator/src/reconciler.rs:136-154`).

### 3. Primary-Agent and Target-Agent Data Flow

The correlated action type carries the boxed `RemoveReplicaIntent`; its
signature participates in the generic durable action identity
(`kuberic-core/src/types.rs:444-496`). The protobuf and conversion layer
encode/decode the remove intent, progress, terminal result, and active v3
correlated action state
(`kuberic-core/proto/kuberic.proto:351-428`,
`kuberic-core/src/grpc/convert.rs:887-1028`,
`kuberic-core/src/grpc/convert.rs:1030-1228`,
`kuberic-core/src/grpc/convert.rs:2048-2197`).

The primary-agent coordinator:

- accepts the exact correlated intent and publishes typed progress
  (`kuberic-core/src/replica_agent.rs:799-878`,
  `kuberic-core/src/replica_agent.rs:2179-2328`);
- installs reduced CatchUp, starts a tracked write-quorum wait, marks
  current-install dispatch before installing reduced Current, and requires
  exact postcondition observation
  (`kuberic-core/src/replica_agent.rs:2330-2588`);
- permits previous-Current compensation only before current-install dispatch
  ambiguity and otherwise treats the state as unsafe
  (`kuberic-core/src/replica_agent.rs:2590-2798`);
- timestamps exact reduced-Current commit and derives retirement expiry
  (`kuberic-core/src/replica_agent.rs:2829-2878`);
- repeatedly removes only the exact target connection, then runs the
  lifecycle-peer retirement and attests clean or degraded completion
  (`kuberic-core/src/replica_agent.rs:2880-3024`).

The lifecycle-peer retire request validates removal-only fields, signed
configuration projection, sender/parent/target authority, target generation,
control version, epoch, and expiry
(`kuberic-core/src/replica_lifecycle.rs:177-340`). The target runtime performs
`ChangeRole(None)` followed by `Close`, and duplicate/restart handling derives
completion from live target postconditions
(`kuberic-core/src/replica_agent.rs:2091-2177`,
`kuberic-core/src/replica_agent.rs:6608-7358`).

The implemented protocol document records the same ownership boundary:
operator/CRD owns topology and publication, the primary agent owns transient
removal coordination, the target agent owns local retirement admission, and
`PodRuntime` owns ordered runtime effects
(`docs/features/kuberic/implemented/agent-owned-replica-remove-protocol.md:20-48`).

### 4. Explicit Remove-Replica Test Matrix and Fixtures

#### Decision-engine unit fixtures

The unit module builds a deterministic three-member snapshot, target
generation, replica observations, a freeze/dispatch helper, correlated action
progress, and a committed-operation helper
(`kuberic-operator/src/durable/remove_replica.rs:1665-1868`).

The tests cover:

- start/admission, version rejection, and malformed commit state
  (`kuberic-operator/src/durable/remove_replica.rs:1870-1984`);
- target-generation drift, deterministic coarse intent, and exact commit
  recording without stable publication
  (`kuberic-operator/src/durable/remove_replica.rs:1986-2139`);
- compensation and ambiguity after current-install dispatch
  (`kuberic-operator/src/durable/remove_replica.rs:2141-2370`);
- commit recognition without retained history, generation redrive, and frozen
  compensation expiry
  (`kuberic-operator/src/durable/remove_replica.rs:2372-2563`);
- resource-version conflict replay, UID-fenced cleanup, exact primary
  connection evidence, retirement evidence, and retirement expiry
  (`kuberic-operator/src/durable/remove_replica.rs:2565-2814`);
- typed restart dispositions, no-op poison persistence, attempt exhaustion,
  and operation-condition/status helpers
  (`kuberic-operator/src/durable/remove_replica.rs:2816-3047`).

#### Reconciler integration fixtures

Reusable helpers advance to an explicit operation phase, coordinator phase,
or persisted dispatch authorization; read live correlated progress/terminal
state; inject lost replies; and assert that the operator dispatched exactly
one coarse remove intent and no fine-grained removal controls
(`examples/kvstore/tests/reconciler.rs:1552-1668`,
`examples/kvstore/tests/reconciler.rs:1850-1865`).

The removal integration matrix covers:

- complete ScaleDown with one lost coarse reply, every persisted operation
  phase, commit-before-publication history, and no fine-grained operator
  commands (`examples/kvstore/tests/reconciler.rs:6343-6468`);
- unreachable-target `Force` completion with degraded retirement
  (`examples/kvstore/tests/reconciler.rs:6470-6539`);
- malformed agent status, status conflict before intent, old-UID cleanup
  protection, ScaleDown preadmission/minimum, same-UID generation drift, and
  no ScaleDown-to-Force conversion
  (`examples/kvstore/tests/reconciler.rs:6541-6909`);
- commit-record and stable-publication resource-version conflict replay without
  duplicate intent or premature topology mutation
  (`examples/kvstore/tests/reconciler.rs:7914-8011`);
- end-to-end healthy scale-down with retained PVC and post-removal data-plane
  write (`examples/kvstore/tests/reconciler.rs:8501-8565`).

#### Core and wire regression fixtures

- Primary coordinator tests cover exact admission, invalid progress,
  lost replies, phase-postcondition resume, compensation/expiry, stale primary
  authority, responsiveness during held calls, post-deadline roll-forward,
  clock budgets, and connection/replacement behavior
  (`kuberic-core/src/replica_agent.rs:5539-6607`).
- Target retirement tests cover ordered role-none/close, duplicate replay,
  transport/protocol rejection, real gRPC, exact sender/parent/epoch/config
  fences, Force generation/control fences, midflight identity drift,
  replacement protection, deadlines, restart boundaries, and bounded
  retention (`kuberic-core/src/replica_agent.rs:6608-7358`).
- Lifecycle request unit tests isolate removal-only signatures and exact
  incarnation absence (`kuberic-core/src/replica_lifecycle.rs:628-739`).
- Conversion tests round-trip strict remove types, reject malformed wire
  combinations, and preserve active v3 action/progress state
  (`kuberic-core/src/grpc/convert.rs:2687-2855`,
  `kuberic-core/src/grpc/convert.rs:2886-3041`).

### 5. Durable-Execution Kernel Boundary

#### Shared kernel contracts

- `Workflow::run` and `WorkflowContext` provide ordinary async replay over a
  linear exact activity history; typed `call` is the workflow-body operation
  (`durable-execution/src/workflow.rs:47-128`).
- `DurableActivity` owns immutable name/version and input/result bounds;
  canonical JSON encoding and decoding enforce those bounds
  (`durable-execution/src/typed.rs:55-181`).
- `PreparedActivityResolver` converts a logical request into an exact
  dispatch specification and validates recorded replay specifications
  (`durable-execution/src/typed.rs:5-52`).
- `CheckpointPayload` separates active activity history from compact terminal
  outcome/count state. Validation enforces execution identity/input/terminal
  bound, configured/admitted capacity, activity count/history shape, maximum
  result reservation, and terminal capacity
  (`durable-execution/src/checkpoint.rs:133-370`).
- `CheckpointStore` is provider-neutral and exposes load plus CAS with
  `Accepted`, `Conflict`, and `OutcomeUnknown`; provider errors represent
  definitely non-applied local/pre-request failures
  (`durable-execution/src/store.rs:68-141`).
- `HostOutcome` is the public boundary vocabulary for accepted schedules,
  permits, observations, completion, quarantine, nondeterminism, checkpoint
  rejection, reload, and store failure
  (`durable-execution/src/host.rs:10-165`).
- `turn_and_expose_with` combines scheduling and exact prepared exposure into
  one CAS and returns a private `DispatchPermit` only after acceptance
  (`durable-execution/src/host.rs:308-449`,
  `durable-execution/src/host.rs:711-761`).
- `observe_and_turn_with` validates the exact exposed logical activity and
  result bound, then combines result persistence with next exposure or compact
  terminal persistence in one CAS
  (`durable-execution/src/host.rs:538-686`,
  `durable-execution/src/host.rs:773-832`).
- Conflict and unknown CAS outcomes map to `ReloadRequired`; a loaded exposed
  activity maps to `Quarantined`, so no new permit is inferred
  (`durable-execution/src/host.rs:356-410`,
  `durable-execution/src/host.rs:848-860`).
- The Kubernetes provider implements the same store contract with deterministic
  per-execution ConfigMap names, opaque `resourceVersion`, data-budget
  enforcement, stable owner relationship, replace/create CAS, and conservative
  mutation classification
  (`durable-execution/src/kubernetes.rs:197-253`,
  `durable-execution/src/kubernetes.rs:282-385`).

#### Switchover-specific host layer

- `PilotCheckpointStore`, `PilotStoreFactory`, `PilotHostKey`, and
  `DurableSwitchoverPilotRuntime` select Kubernetes/in-memory stores, cache one
  host per owner/execution, retain completed measurement snapshots, and build a
  `DurableHost<MeasuredPilotCheckpointStore>`
  (`kuberic-operator/src/durable/pilot.rs:63-107`,
  `kuberic-operator/src/durable/pilot.rs:109-235`).
- The workflow input, compact mutable state, activity input/kind, activity
  output, terminal type, and terminal records are all named and shaped around
  switchover (`kuberic-operator/src/durable/pilot.rs:238-408`).
- The workflow delegates state interpretation to the existing explicit
  switchover decision engine, fuses effect-free `Persist` transitions in
  memory, schedules passive activities, bounds one proven-no-admission
  redelivery per action, and encodes terminal accounting
  (`kuberic-operator/src/durable/pilot.rs:409-616`).
- `new_pilot_reference` calls `start_switchover`, serializes that initial
  operation, and emits a `DurableSwitchoverPilotStatus`; admission projects
  switchover success and rollback transcripts
  (`kuberic-operator/src/durable/pilot.rs:1379-1410`,
  `kuberic-operator/src/durable/pilot.rs:1494-1550`).
- The prepared resolver calls `evaluate_adapter_step`, converts Execute/label
  decisions into exact prepared commands, and accepts recorded commands only
  when their complete specification validates
  (`kuberic-operator/src/durable/pilot.rs:898-943`,
  `kuberic-operator/src/durable/pilot.rs:980-1157`).

### 6. Feature, CRD, and Reconciler Routing

- The operator's default feature set is empty. The only durable operator
  feature is `durable-switchover-pilot`, which enables the kernel Kubernetes
  feature and `rand` (`kuberic-operator/Cargo.toml:11-18`).
- Pilot modules, imports, state fields, constructors, and main-process runtime
  creation are compile-time gated
  (`kuberic-operator/src/durable/mod.rs:15-18`,
  `kuberic-operator/src/reconciler.rs:40-48`,
  `kuberic-operator/src/reconciler.rs:64-126`,
  `kuberic-operator/src/main.rs:34-40`).
- Runtime selection currently exists only for switchover:
  `KubericSetSpec.switchover_execution_mode` defaults to `Explicit`, with
  `DurablePilot` as the opt-in value
  (`kuberic-operator/src/crd.rs:43-55`,
  `kuberic-operator/src/crd.rs:184-191`).
- The durable pilot status reference is a separate
  `KubericSetStatus.durable_switchover_pilot` containing version, execution ID,
  deterministic checkpoint name, and exact initial-operation JSON
  (`kuberic-operator/src/crd.rs:118-127`,
  `kuberic-operator/src/crd.rs:193-209`).
- Unsupported pilot selection is rejected by
  `validate_new_switchover_engine`; it does not fall back to explicit
  (`kuberic-operator/src/reconciler.rs:190-204`). Unit tests cover explicit
  availability and no silent fallback
  (`kuberic-operator/src/reconciler.rs:4218-4241`).
- Healthy-phase switchover routing first validates the selected engine. Pilot
  selection persists only the immutable pilot reference and clears the
  explicit operation; explicit selection persists `start_switchover` and
  clears the pilot reference
  (`kuberic-operator/src/reconciler.rs:1338-1424`).
- In `Phase::Switchover`, status with no explicit operation and a pilot
  reference routes to `reconcile_durable_switchover_pilot`; all explicit
  switchovers and all add/remove operations route to
  `reconcile_durable_operation`
  (`kuberic-operator/src/reconciler.rs:1613-1656`).
- CRD tests verify omission defaults to explicit and generated/deployed schema
  contains the pilot fields
  (`kuberic-operator/src/crd.rs:1007-1049`).
- Integration tests separately cover default explicit switchover and opt-in
  pilot. The pilot happy path also requests a later normal switchover and
  verifies it starts an explicit operation rather than reusing the retained
  pilot reference
  (`examples/kvstore/tests/reconciler.rs:3563-3764`,
  `examples/kvstore/tests/reconciler.rs:3766-4008`).

There is currently no remove-replica execution-mode field, durable-removal
status reference, durable-removal Cargo feature, or kernel-hosted removal
reconciler route. Existing remove selection always creates
`KubericSetStatus.operation` and enters the explicit operation reconciler
(`kuberic-operator/src/crd.rs:24-60`,
`kuberic-operator/src/crd.rs:109-127`,
`kuberic-operator/src/reconciler.rs:1477-1518`,
`kuberic-operator/src/reconciler.rs:1646-1656`).

### 7. Effect Adapter Extension Points

#### Already shared by explicit and pilot paths

- `ReplicaEffectCommand` is an exact persisted correlated command containing
  action identity/signature, target identity, postcondition, agent generation,
  control version, observed runtime epoch, and encoded action
  (`kuberic-operator/src/durable/effects.rs:38-81`).
- `LabelEffectCommand` binds replica ID, Pod name, exact UID, role, and an
  identity signature (`kuberic-operator/src/durable/effects.rs:83-125`).
- `DurableEffectOutcome` distinguishes applied, definite failure,
  proven-no-admission, pending, and unknown outcomes
  (`kuberic-operator/src/durable/effects.rs:127-163`).
- Dispatch evidence freezing, exact command reconstruction, correlated
  execution, acknowledgement validation, and failure classification are in
  shared functions outside the pilot workflow body
  (`kuberic-operator/src/durable/effects.rs:187-260`,
  `kuberic-operator/src/durable/effects.rs:295-589`).

#### Current switchover-specific adapter boundaries

- `validate_pilot_replica_action_kind` recognizes only the seven switchover
  ReplicaAgent action classes
  (`kuberic-operator/src/durable/effects.rs:262-289`).
- `evaluate_adapter_step` converts only `Persist`, `Complete`, `Wait`,
  `Execute`, `PatchPodRole`, and `PatchPodRoleExactUid`; other explicit
  decisions return an unsupported-decision error
  (`kuberic-operator/src/durable/pilot.rs:898-943`).
- Explicit remove-replica additionally produces
  `RecordCommitEvidence`, `DeletePod`, and `CommitSnapshot`
  (`kuberic-operator/src/durable/remove_replica.rs:766-802`,
  `kuberic-operator/src/durable/remove_replica.rs:954-1007`).
- `PilotActivityKind` contains passive observation, prepared replica command,
  and prepared label command only
  (`kuberic-operator/src/durable/pilot.rs:287-307`).
- The effect bridge consumes one permit, dispatches replica or label commands,
  maps proven non-admission to an observation, and otherwise leaves uncertain
  exposure awaiting authoritative evidence
  (`kuberic-operator/src/durable/effects.rs:589-676`).
- Quarantine resolution is expressed in terms of
  `DurableSwitchoverStepResult`, `PilotAdapterDecision`, and switchover
  postcondition transitions
  (`kuberic-operator/src/durable/effects.rs:679-811`).

### 8. Checkpoint Store and Measurement Extension Points

- The kernel `CheckpointStore` contract and Kubernetes implementation are not
  workflow-specific (`durable-execution/src/store.rs:106-141`,
  `durable-execution/src/kubernetes.rs:197-385`).
- `PilotCheckpointStore` is a thin workflow-named enum over Kubernetes and
  in-memory providers (`kuberic-operator/src/durable/pilot.rs:63-105`).
- `MeasuredPilotCheckpointStore` wraps a store for one execution and records
  load/write attempts, accepted writes, conflict/unknown/failure counts,
  authoritative/active/terminal sizes, completed activity count, and
  external/passive split
  (`kuberic-operator/src/durable/pilot_store.rs:18-58`,
  `kuberic-operator/src/durable/pilot_store.rs:97-123`,
  `kuberic-operator/src/durable/pilot_store.rs:246-381`).
- Active-checkpoint classification decodes `PilotActivityKind` and counts only
  `PassiveObservation`, `PreparedReplica`, and `PreparedLabel`; terminal
  classification calls the switchover terminal accounting decoder
  (`kuberic-operator/src/durable/pilot_store.rs:164-239`).
- Boundary logging and all measurement messages are named “durable
  switchover”; the wrapper imports pilot activity and terminal decoders
  (`kuberic-operator/src/durable/pilot_store.rs:13-16`,
  `kuberic-operator/src/durable/pilot_store.rs:126-162`,
  `kuberic-operator/src/durable/pilot_store.rs:271-379`).
- The reconciler correlates every host outcome with the measurement store and
  enriches terminal activity accounting before validation/publication
  (`kuberic-operator/src/reconciler.rs:2725-2913`,
  `kuberic-operator/src/reconciler.rs:3066-3094`).
- Terminal reload is checked before workflow polling, and terminal publication
  occurs only from a decoded/validated terminal checkpoint
  (`kuberic-operator/src/reconciler.rs:2649-2661`,
  `kuberic-operator/src/reconciler.rs:3043-3064`,
  `kuberic-operator/src/reconciler.rs:3096-3234`).

### 9. Complexity Measurement: Current Charged Scopes

The script measures executable non-comment lines and lexical decision points
matching `if`, `match`, `for`, `while`, `&&`, or `||`
(`scripts/measure-switchover-complexity.py:8-10`,
`scripts/measure-switchover-complexity.py:102-109`).

| Script label | Current source boundary | Marker/file evidence | Attribution in current summary |
|---|---|---|---|
| `explicit_switchover` | Explicit CRD workflow | `kuberic-operator/src/durable/switchover.rs:32-1525` | Separate explicit baseline |
| `pilot_module` | Entire switchover pilot module | `kuberic-operator/src/durable/pilot.rs:47-2081` | Charged workflow/hosting module |
| `pilot_workflow_subset` | Comparable nested pilot scope | `kuberic-operator/src/durable/pilot.rs:409-1378` | Diagnostic only; not added |
| `pilot_workflow_body` | Async orchestration body | `kuberic-operator/src/durable/pilot.rs:412-616` | Diagnostic only; not added |
| `shared_operator_effect_adapters` | Effect preparation/dispatch/recovery | `kuberic-operator/src/durable/effects.rs:37-849` | Shared reusable infrastructure |
| `pilot_store_integration` | Measured checkpoint wrapper | `kuberic-operator/src/durable/pilot_store.rs:18-382` | Operator integration |
| `pilot_effect_bridge_integration` | Reconciler bridge wrapper | `kuberic-operator/src/reconciler.rs:372-456` | Operator integration |
| `pilot_reconcile_integration` | Pilot reconcile/terminal publication | `kuberic-operator/src/reconciler.rs:2633-3273` | Operator integration |
| `shared_kernel_typed` | Complete typed activity module | `durable-execution/src/typed.rs:1-181` | Shared reusable infrastructure |
| `shared_kernel_fused` | Fused expose and observe ranges | `durable-execution/src/host.rs:308-449`, `durable-execution/src/host.rs:538-686` | Shared reusable infrastructure |

The declarations are in `scripts/measure-switchover-complexity.py:13-50`.
Marker extraction rejects missing or reversed boundaries
(`scripts/measure-switchover-complexity.py:53-88`). Charged-scope validation
tracks every `(path,line)` and rejects intersections
(`scripts/measure-switchover-complexity.py:90-100`). The charged list is
`pilot_module + shared_operator_effect_adapters + pilot_store_integration +
pilot_effect_bridge_integration + pilot_reconcile_integration +
shared_kernel_typed + shared_kernel_fused`
(`scripts/measure-switchover-complexity.py:112-131`).

`pilot_workflow_subset` and `pilot_workflow_body` are nested diagnostics. The
script computes:

- `shared = effect adapters + typed kernel + fused kernel`;
- `integration = measured store + effect bridge + pilot reconcile`;
- `pilot total = pilot module + shared + integration`; and
- `combined = pilot total + explicit switchover`
  (`scripts/measure-switchover-complexity.py:133-164`).

At commit `740ee5119327d21ab5d7987bb1fab0df522096be`, the script emits:

| Scope | Executable lines | Decision points |
|---|---:|---:|
| Workflow body only | 201 | 29 |
| Comparable nested workflow scope | 930 | 108 |
| Shared reusable infrastructure | 1,208 | 110 |
| Operator integration | 1,047 | 55 |
| Pilot non-overlapping total | 4,161 | 326 |
| Explicit switchover | 1,449 | 172 |
| Combined explicit/shared/pilot total | 5,610 | 498 |

The same current values are documented at
`docs/features/kuberic/durable-execution-roadmap.md:205-210`.

#### Design-neutral before/after shared-growth map

The existing source provides these attribution facts without assigning any
future code location:

| Accounting term from the specification | Current evidence base |
|---|---|
| `legacy_remove` | The explicit remove implementation currently occupies `kuberic-operator/src/durable/remove_replica.rs:1-3047`; the file contains both implementation and in-file tests, so an exact comparable production marker does not yet exist. |
| `remove_body` | No kernel-hosted remove workflow body or marker exists at this revision. |
| `remove_integration` | No remove-specific kernel routing/status/store/telemetry marker exists at this revision. |
| `shared_before` | The currently charged shared labels are `shared_operator_effect_adapters`, `shared_kernel_typed`, and `shared_kernel_fused` (`scripts/measure-switchover-complexity.py:28-49`, `scripts/measure-switchover-complexity.py:137-141`). Their current value is 1,208/110. |
| `shared_after` | This is the same logical shared label set measured after code serves both workflows; the current script has no historical-revision loader or before/after calculation. |
| `shared_growth` | The specification defines it as `shared_after - shared_before` (`.paw/work/durable-remove-replica/Spec.md:295-313`); no current script output computes it. |
| Charged overlap | Current overlap validation is line-based across the supplied charged labels and includes whole-file segments when no marker is specified (`scripts/measure-switchover-complexity.py:70-100`). |
| Nested diagnostics | Current nested pilot body/subset reports are deliberately excluded from additive totals (`scripts/measure-switchover-complexity.py:147-149`, `scripts/measure-switchover-complexity.py:171-176`). |
| Historical constants | The script embeds four original pilot baseline constants, separate from current measured values (`scripts/measure-switchover-complexity.py:165-168`). |

This map separates three existing categories: workflow module, shared reusable
scope, and operator integration. The current repository has no marker that
classifies future remove-specific code and no exact production-only marker
inside the explicit remove file.

### 10. Feasibility Predicate and Test-Fixture Patterns

#### Current predicate and classifier

- The feasibility evidence test derives authoring predicates from source
  markers and `include_str!`, runs all 45 registered scenarios, derives
  provider/bounds/terminal predicates from scenario IDs, and prints every
  predicate and classifier input
  (`durable-execution/tests/feasibility.rs:44-171`,
  `durable-execution/tests/feasibility.rs:207-302`).
- Runtime neutrality currently requires async store/host APIs and then checks
  `!crate_manifest.contains("tokio")` over the complete manifest
  (`durable-execution/tests/feasibility.rs:174-181`).
- The manifest's library dependencies contain no async runtime, while
  `tokio.workspace = true` is under `[dev-dependencies]`
  (`durable-execution/Cargo.toml:10-29`).
- The classifier is a fixed truth table: safety/determinism failure is
  `Infeasible`; otherwise conformance failure, authoring failure, or any
  in-scope limitation is `ConditionallyFeasible`; only all-pass/no-limitation
  is `Feasible` (`durable-execution/src/assessment.rs:1-29`).
- The exhaustive classifier test iterates every Boolean combination and checks
  that same rule (`durable-execution/tests/feasibility.rs:304-330`;
  crate-level duplicate truth-table coverage is at
  `durable-execution/src/assessment.rs:31-83`).

The targeted evidence command at this revision reports:

```text
REVISION status=fail predicate="async provider and host contract is runtime neutral"
REVISION status=pass predicate="provider failure and uncertainty scenarios pass"
REVISION status=pass predicate="bounded checkpoint and base64 scenarios pass"
REVISION status=pass predicate="completion-only terminal lifecycle scenarios pass"
REVISION status=pass predicate="kernel scope and deferred roadmap are documented"
INPUT safety_and_determinism_pass=true all_conformance_pass=true authoring_simplicity_pass=true has_in_scope_limitation=true
CLASSIFICATION value=conditionally feasible
```

#### Existing fixture conventions relevant to the predicate

- Repository-source evidence is embedded at compile time with `include_str!`
  (`durable-execution/tests/feasibility.rs:72-88`,
  `durable-execution/tests/feasibility.rs:174-186`).
- A source range is delimited by stable comment markers and extracted with
  `split_once`, making the selected authoring body itself a fixture
  (`durable-execution/tests/feasibility.rs:18-32`,
  `durable-execution/tests/feasibility.rs:72-79`).
- Predicate evidence is represented as small `(name, passed)` records and
  arrays, then printed uniformly
  (`durable-execution/tests/feasibility.rs:34-38`,
  `durable-execution/tests/feasibility.rs:108-132`,
  `durable-execution/tests/feasibility.rs:186-205`,
  `durable-execution/tests/feasibility.rs:239-252`).
- Scenario fixtures use one stable enum registry, contiguous stable IDs, and
  table-driven selection by ID
  (`durable-execution/tests/support/scenarios.rs:18-150`,
  `durable-execution/tests/support/scenarios.rs:210-268`).
- Replay fixtures use compact constructor helpers for execution IDs, names,
  bytes, specs, limits, and envelopes, followed by one test per mismatch or
  lifecycle property (`durable-execution/tests/replay.rs:20-102`,
  `durable-execution/tests/replay.rs:104-381`).
- Current feasibility tests do not contain separate manifest string fixtures
  for library versus development dependencies; the only manifest fixture is
  the real `durable-execution/Cargo.toml` embedded in full
  (`durable-execution/tests/feasibility.rs:174-181`).

### 11. Switchover Regression Patterns Available as Host Examples

- Pilot unit tests cover stable/distinct references, owner scope, replica
  admission, projected history/checkpoint budgets, malformed identity, host
  caching, workflow replay and terminal reload, restart quarantine, one-use
  permit guards, full dispatch-fence binding, semantic replay mismatch,
  bounded non-admission redelivery, terminal/accounting validation, and
  schedule/observation/exposure fault reload
  (`kuberic-operator/src/durable/pilot.rs:2120-4008`).
- Generic replay tests cover typed canonical input, exact prepared replay,
  complete specification mismatch, completed prepared activity replay, and
  prepared failure classes
  (`durable-execution/tests/replay.rs:667-1107`).
- Shared effect tests distinguish portable outcome kinds and dispatch failure
  classes, exercise all seven switchover replica commands, and reject
  preparation under identity/protocol drift
  (`kuberic-operator/src/durable/effects.rs:971-1177`).
- Measurement-store tests cover authoritative versus unknown accounting,
  terminal-only accounting reconstruction, impossible accounting shapes, and
  bounded event history
  (`kuberic-operator/src/durable/pilot_store.rs:520-840`).
- Kubernetes store tests cover exact opaque revisions, ownership, conflicts,
  conservative mutation classification, data budgets, and malformed reload
  (`durable-execution/tests/kubernetes_checkpoint.rs:189-936`).
- Pilot reconciler tests cover the happy path, terminal redelivery, every-turn
  restart, compensation, lost effect replies, terminal reload after status
  failure, terminal CAS conflict, stale target incarnation, unknown checkpoint
  outcomes, and distinct repeated execution identity
  (`examples/kvstore/tests/reconciler.rs:3766-4898`).

## Architecture Documentation

### Existing authority split

- **CRD status**: sole durable global authority for the explicit remove
  operation, commit evidence, cleanup facts, and stable publication
  (`kuberic-operator/src/crd.rs:109-127`,
  `kuberic-operator/src/crd.rs:296-499`).
- **Primary ReplicaAgent**: volatile, generation-qualified coordinator for
  reduced configuration, quorum, connection removal, target retirement, and
  attestation (`kuberic-core/src/replica_agent.rs:2179-3280`).
- **Target ReplicaAgent/lifecycle peer**: exact local retirement admission and
  ordered role-none/close effects
  (`kuberic-core/src/replica_lifecycle.rs:177-340`,
  `kuberic-core/src/replica_agent.rs:2091-2177`).
- **Operator reconciler**: desired-topology selection, operation persistence,
  exact observation gathering, correlated dispatch, Kubernetes cleanup, and
  final status publication
  (`kuberic-operator/src/reconciler.rs:1477-1518`,
  `kuberic-operator/src/reconciler.rs:3274-3747`).

### Existing durable pilot authority split

- **CRD status** persists only immutable execution authority/reference before
  checkpoint creation (`kuberic-operator/src/durable/pilot.rs:1379-1410`,
  `kuberic-operator/src/reconciler.rs:1365-1401`).
- **Checkpoint** owns active workflow progress, exact prepared effects,
  observations, and terminal outcome (`durable-execution/src/checkpoint.rs:133-239`).
- **Kernel host** owns replay, CAS boundary handling, permit issuance,
  quarantine, result reservation, and terminal compaction
  (`durable-execution/src/host.rs:174-860`).
- **Operator adapter/reconciler** owns authoritative cluster observation,
  external effect execution, quarantine resolution, and final CRD publication
  (`kuberic-operator/src/durable/effects.rs:589-811`,
  `kuberic-operator/src/reconciler.rs:2635-3234`).

## Code References

- `kuberic-operator/src/durable/remove_replica.rs:51-146` — explicit removal
  admission and initial operation.
- `kuberic-operator/src/durable/remove_replica.rs:152-1017` — explicit removal
  decision flow from freeze through publication.
- `kuberic-operator/src/reconciler.rs:1477-1518` — healthy ScaleDown selection.
- `kuberic-operator/src/reconciler.rs:3274-3747` — explicit operation
  observation, decision, dispatch, persistence, cleanup, and completion.
- `kuberic-core/src/replica_agent.rs:2179-3280` — primary-agent removal
  coordinator and target retirement.
- `kuberic-operator/src/durable/pilot.rs:409-616` — existing kernel-hosted
  switchover workflow body.
- `kuberic-operator/src/durable/pilot.rs:898-1195` — switchover adapter,
  prepared resolver, and permit guard.
- `kuberic-operator/src/durable/effects.rs:37-849` — charged shared effect
  adapter scope.
- `kuberic-operator/src/durable/pilot_store.rs:18-382` — charged measured store
  integration scope.
- `kuberic-operator/src/reconciler.rs:2633-3273` — charged pilot reconcile
  integration scope.
- `durable-execution/src/host.rs:308-449` — charged fused turn/exposure scope.
- `durable-execution/src/host.rs:538-686` — charged fused
  observation/progression scope.
- `scripts/measure-switchover-complexity.py:13-181` — measurement declaration,
  extraction, overlap validation, and totals.
- `durable-execution/tests/feasibility.rs:174-205` — current runtime-neutrality
  predicate and revision evidence.
- `.github/workflows/CI.yml:43-78` — repository CI validation sequence.

GitHub permalinks are omitted because the current commit is not reported by
`git branch -r --contains HEAD`; file:line references are the primary evidence.

## Open Questions

1. The repository does not yet contain an exact production-only
   `legacy_remove` complexity marker. The explicit remove file combines
   implementation (`kuberic-operator/src/durable/remove_replica.rs:1-1664`)
   and in-file tests (`kuberic-operator/src/durable/remove_replica.rs:1665-3047`).
2. No names or source boundaries yet exist for the dedicated durable-removal
   Cargo feature, CRD runtime selection/reference, workflow body, integration
   scope, or measurement markers. The specification defines their required
   semantics but not their final identifiers
   (`.paw/work/durable-remove-replica/Spec.md:188-248`,
   `.paw/work/durable-remove-replica/Spec.md:295-328`).
3. The current complexity script stores original pilot baseline constants but
   does not obtain `shared_before` from a revision or artifact. The provenance
   mechanism for the required before/after shared-growth comparison is not
   present in current code
   (`scripts/measure-switchover-complexity.py:165-168`).
4. The current feasibility test has no independent library-dependency and
   development-dependency manifest fixtures; it embeds only the real complete
   manifest (`durable-execution/tests/feasibility.rs:174-181`).
