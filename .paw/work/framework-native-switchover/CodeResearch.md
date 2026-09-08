---
date: 2026-09-08T16:52:34.057+00:00
git_commit: 01773d8f84bc8f08318d92ad2e974180803f5a97
branch: feature/framework-native-switchover
repository: github.com/youyuanwu/kuberic
topic: "Framework-Native Switchover Graduation"
tags: [research, codebase, switchover, durable-execution, operator]
status: complete
last_updated: 2026-09-08
---

# Research: Framework-Native Switchover Graduation

## Research Question

Where and how are the current explicit and feature-gated durable-pilot
switchover paths selected, admitted, recovered, executed, quarantined,
measured, tested, and published; which shared runner and ConfigMap-provider
interfaces already host the pilot; which compatibility precedents exist; and
what concrete code and protocol evidence bears on retaining independently
correlated local actions rather than introducing a coarse switchover intent?

## Summary

The current tree exposes two switchover engines. `spec.switchoverExecutionMode`
defaults to `explicit`, while `durablePilot` additionally requires the
default-off `durable-switchover-pilot` Cargo feature
(`kuberic-operator/src/crd.rs:50-56`,
`kuberic-operator/src/crd.rs:194-203`,
`kuberic-operator/Cargo.toml:10-14`). Admission writes either the explicit
`status.operation` or the pilot `status.durableSwitchoverPilot`; recovery later
routes from those persisted status shapes rather than from the current
selector (`kuberic-operator/src/reconciler.rs:1521-1597`,
`kuberic-operator/src/reconciler.rs:1788-1820`).

Both engines use the same pure switchover state machine. It persists and
observes write revocation, frozen-LSN capture, target catch-up, demotion,
promotion, retained-member epoch convergence, CatchUp and Current
configuration installation, exact-UID routing labels, final attestation, and
two compensation shapes (`kuberic-operator/src/durable/switchover.rs:106-419`,
`kuberic-operator/src/durable/switchover.rs:449-598`). The pilot wraps those
decisions in typed durable activities and a compact mutable projection while
retaining the complete immutable initial operation in both the CRD reference
and workflow input (`kuberic-operator/src/durable/pilot.rs:155-283`,
`kuberic-operator/src/durable/pilot.rs:1738-1819`).

The shared framework-native runner and provider are already production code
for remove-replica and are already consumed by the optional switchover pilot.
They own authoritative load, terminal short-circuit, fused exact-command
exposure, one-use permits, quarantine, observation/progression, reload
classification, bounded host-outcome fuel, ConfigMap compare-and-swap,
owner-reference enforcement, and measurements
(`kuberic-operator/src/durable/runner.rs:105-175`,
`kuberic-operator/src/durable/runner.rs:218-505`,
`kuberic-operator/src/durable/workflow_host.rs:20-185`,
`kuberic-operator/src/durable/checkpoint_store.rs:18-113`).
Switchover-specific authority remains in `SwitchoverRunnerAdapter`: replica
and Pod observation collection, exact command derivation, effect and
quarantine interpretation, deadline selection, terminal validation, and the
publication handoff (`kuberic-operator/src/durable/pilot.rs:1142-1496`).

The current control contracts support the specification's decision to retain
independently correlated local actions. `DurableReplicaAction` contains
coarse `AddReplicaIntent` and `RemoveReplicaIntent` variants, but switchover is
represented by separate revoke, role, epoch, quorum, and configuration
actions (`kuberic-core/src/types.rs:448-489`). The wire oneof has the same
shape and no switchover intent (`kuberic-core/proto/kuberic.proto:251-271`).
The lifecycle peer supports only `AddBuild` and `Remove`, with
Prepare/Activate/Cleanup/Retire stages
(`kuberic-core/src/replica_lifecycle.rs:125-180`,
`kuberic-core/proto/kuberic.proto:461-472`). Switchover also includes
operator-owned exact-UID Kubernetes label mutations, so its durable authority
currently spans the old primary, target, retained replicas, and Kubernetes
routing rather than one local agent (`kuberic-operator/src/durable/switchover.rs:901-1075`,
`kuberic-operator/src/durable/effects.rs:102-147`).

The current three-member pilot baseline is exactly nine external effects,
three passive observations, 12 completed durable boundaries, and 13 accepted
checkpoint writes (`kuberic-operator/src/durable/pilot.rs:4428-4503`,
`examples/kvstore/tests/reconciler.rs:3705-3762`). Its maximum projected
success and rollback histories are 19 and 21 activities respectively
(`kuberic-operator/src/durable/pilot.rs:4506-4537`). Current pilot bounds use
one 752 KiB encoded ceiling for both active and terminal checkpoints, whereas
the framework-native remove precedent demonstrates separate active, terminal,
payload, input, result, record, and one-unit-over checks
(`kuberic-operator/src/durable/pilot.rs:68-76`,
`kuberic-operator/src/durable/remove_replica_execution.rs:71-83`,
`kuberic-operator/src/durable/remove_replica_execution.rs:4786-4863`).

## Documentation System

- **Framework**: Plain checked-in Markdown. The repository contains no
  MkDocs, Docusaurus, Sphinx, or mdBook navigation/build configuration.
- **Docs Directory**: `docs/`; the main implementation documents are under
  `docs/features/kuberic/`, with protocol decisions under
  `docs/features/kuberic/implemented/` (`README.md:65-72`).
- **Navigation Config**: None. The root README manually links the core
  protocol, operator, user API, design-gap, and testing documents
  (`README.md:65-72`).
- **Style Conventions**: Markdown documents use descriptive headings, tables,
  bullet lists, protocol sequence blocks, and fenced command examples
  (`docs/features/kuberic/operator.md:1-54`,
  `docs/features/kuberic/protocols.md:118-166`,
  `docs/features/kuberic/testing.md:198-220`).
- **Build Command**: No separate prose build exists. Rust documentation tests
  use `cargo test --doc --workspace`
  (`docs/features/kuberic/testing.md:198-208`).
- **Standard Files**: `README.md`, `kuberic-operator/README.md`,
  `durable-execution/README.md`, `docs/Dev.md`, and `docs/Internals.md`.
  No `CHANGELOG.md` or `CONTRIBUTING.md` is present in the repository.

## Verification Commands

- **Type/build check**: `cargo check --all-targets`
  (`.github/workflows/CI.yml:41-42`).
- **Formatting**: `cargo fmt --all -- --check`
  (`.github/workflows/CI.yml:44-47`).
- **Lint**: `cargo clippy --all-targets -- -D warnings`
  (`.github/workflows/CI.yml:49-50`).
- **Current switchover projection and measurement gates**:
  `cargo test -p kuberic-operator --features durable-switchover-pilot success_and_rollback_transcripts_fit_with_redelivery_headroom`;
  `cargo test -p kuberic-operator --features durable-switchover-pilot maximum_projected_history_fits_both_budgets`;
  `cargo test -p kuberic-operator --features durable-switchover-pilot measurements_ -- --nocapture`;
  and
  `cargo test -p kvstore --test reconciler test_durable_execution_switchover_pilot_happy_path -- --nocapture`
  (`.github/workflows/CI.yml:52-60`).
- **Complete pilot reconciler matrix**:
  `CARGO_BUILD_JOBS=2 CARGO_INCREMENTAL=0 RUST_MIN_STACK=4194304 cargo test -p kvstore --test reconciler test_durable_execution_switchover_pilot_ -- --nocapture`
  (`docs/features/kuberic/testing.md:390-408`,
  `docs/features/kuberic/testing.md:470-478`).
- **Meaningful non-cluster suites**:
  `cargo test -p kuberic-core -p kuberic-operator -p kvstore -p sqlite-replicated`
  (`docs/features/kuberic/testing.md:198-205`).
- **Documentation tests**: `cargo test --doc --workspace`
  (`docs/features/kuberic/testing.md:206-208`).
- **Deterministic ConfigMap-provider tests**:
  `cargo test -p kuberic-durable-execution --features kubernetes --test kubernetes_checkpoint`;
  the conditional real-cluster form is
  `cargo test -p kuberic-durable-execution --features kubernetes --test kubernetes_checkpoint_real -- --nocapture`
  (`docs/features/kuberic/testing.md:480-486`).
- **Build**: `cargo build --all-targets`
  (`.github/workflows/CI.yml:73-74`).
- **Full workspace gate**: `cargo test --all --all-features`
  (`.github/workflows/CI.yml:84-86`).
- **Image/deployment surface**: `just images` builds workspace binaries,
  builds the operator and kvstore images, and loads them into KinD
  (`justfile:15-39`, `.github/workflows/CI.yml:76-82`).

## Detailed Findings

### 1. Selector, Feature, Status, Condition, and Resource Surfaces

#### Public selector and schema

- `KubericSetSpec.switchover_execution_mode` is a public serialized field.
  Its documentation says omitted values remain on the explicit CRD-backed
  state machine and the pilot also requires a build feature
  (`kuberic-operator/src/crd.rs:46-56`).
- `SwitchoverExecutionMode` serializes as camelCase and contains
  `Explicit` as its default plus `DurablePilot`
  (`kuberic-operator/src/crd.rs:194-203`).
- The checked-in CRD exposes `spec.switchoverExecutionMode` with default
  `explicit` and enum values `explicit` and `durablePilot`
  (`kuberic-operator/deploy/deployment.yaml:96-114`).
- Schema tests assert both the default and pilot round-trip and require the
  generated and checked-in deployment schemas to contain the selector and
  pilot status fields (`kuberic-operator/src/crd.rs:1082-1117`).

#### Build and internal feature surfaces

- `durable-switchover-pilot` is a default-off operator feature
  (`kuberic-operator/Cargo.toml:10-14`).
- The pilot module is compiled only under that feature
  (`kuberic-operator/src/durable/mod.rs:12-18`).
- Reconciler imports, runtime storage, constructors, test-only kernel imports,
  and pilot entry points are feature-gated
  (`kuberic-operator/src/reconciler.rs:19-52`,
  `kuberic-operator/src/reconciler.rs:75-170`).
- Switchover-specific effect imports, command validation, effect bridging, and
  quarantine code are feature-gated
  (`kuberic-operator/src/durable/effects.rs:18-34`,
  `kuberic-operator/src/durable/effects.rs:946-1138`).
- `MeasuredDurableCheckpointStore::new` selects a native-remove decoder when
  the feature is absent and a switchover decoder when it is present; explicit
  decoder construction remains available through `with_decoder`
  (`kuberic-operator/src/durable/checkpoint_store.rs:186-239`).
- The kvstore crate enables the operator pilot feature only in its development
  dependency (`examples/kvstore/Cargo.toml:21-27`). The ordinary `just images`
  path builds workspace binaries without a feature argument
  (`justfile:15-29`).
- CI contains a dedicated pilot feature gate and separately runs the
  all-features workspace suite (`.github/workflows/CI.yml:52-60`,
  `.github/workflows/CI.yml:84-86`).

#### Persisted status and condition surfaces

- The explicit path stores the full `DurableOperationStatus` in
  `status.operation`; the pilot stores an immutable version, execution ID,
  deterministic checkpoint name, and exact initial-operation JSON in
  `status.durableSwitchoverPilot`
  (`kuberic-operator/src/crd.rs:115-139`,
  `kuberic-operator/src/crd.rs:205-215`).
- `DurableOperationStatus` is a shared schema for create, switchover, add,
  legacy remove, and failover. For switchover it carries operation/execution
  identity, version/kind/phase, previous and target snapshots, old and target
  primaries, frozen LSN, secondary cursor, deadline, one pending action, and
  bounded error state (`kuberic-operator/src/crd.rs:322-391`).
- `PendingActionStatus` persists deterministic action identity, sequence,
  action kind, exact target replica and incarnation, expected epoch,
  postcondition, attempts/deadline/error, dispatch authorization, agent
  generation, control version, observed runtime epoch, and the exact encoded
  action payload (`kuberic-operator/src/crd.rs:794-819`).
- Explicit operations publish a `DurableOperation` condition; active,
  completed, compensated/safe-failure, poisoned, and specialized operation
  waits share this condition type (`kuberic-operator/src/durable/mod.rs:163-294`).
- Pilot progress publishes a separate `DurableSwitchoverPilot` condition with
  `Accepted`, runner wait/reload/storage states, `Completed`,
  `CompensatedOrSafeFailure`, `Quarantined`, or the outer `Blocked` wrapper
  (`kuberic-operator/src/reconciler.rs:1566-1574`,
  `kuberic-operator/src/reconciler.rs:2864-2920`,
  `kuberic-operator/src/reconciler.rs:2922-2957`,
  `kuberic-operator/src/reconciler.rs:4141-4157`).

#### Kubernetes resource surfaces

- Pilot checkpoints are ConfigMaps named
  `kuberic-checkpoint-<32 lowercase hex execution ID>`, labeled
  `kuberic.io/component=durable-checkpoint`, with the complete envelope in
  `data["checkpoint.json"]`
  (`durable-execution/src/kubernetes.rs:32-39`,
  `durable-execution/src/kubernetes.rs:242-277`).
- Switchover constructs one non-controlling, non-blocking owner reference to
  the exact namespaced `KubericSet` UID
  (`kuberic-operator/src/durable/pilot.rs:1830-1852`).
- Provider construction rejects empty owner fields, controlling ownership,
  blocking owner deletion, or a namespaced owner in another namespace; load
  and replacement reject a changed owner relationship
  (`durable-execution/src/kubernetes.rs:281-306`,
  `durable-execution/src/kubernetes.rs:340-367`,
  `durable-execution/src/kubernetes.rs:424-476`).
- The operator deployment grants ConfigMap `get`, `create`, and `update`, but
  not delete (`kuberic-operator/deploy/deployment.yaml:2478-2515`).
  Standalone writer and cleanup manifests keep writer and orphan-cleanup
  permissions separate
  (`durable-execution/deploy/checkpoint-writer-rbac.yaml:1-25`,
  `durable-execution/deploy/checkpoint-cleanup-rbac.yaml:1-33`).
- The checked-in kvstore `KubericSet` omits the selector, so current default
  deserialization selects explicit execution
  (`examples/kvstore/deploy/kubericset.yaml:1-14`,
  `kuberic-operator/src/crd.rs:194-203`).

### 2. Admission, Recovery Routing, and Publication

#### Explicit admission

- Healthy reconciliation detects a requested primary change by comparing
  `status.currentPrimary` and `status.targetPrimary`, validates the chosen
  engine, resolves the target from current Pods, requires the target in the
  committed driver topology, and snapshots the current driver
  (`kuberic-operator/src/reconciler.rs:1516-1545`).
- Unless the feature-gated pilot branch is selected, admission calls
  `start_switchover`, writes `phase=Switchover`,
  `status.operation=Some(operation)`, clears
  `status.durableSwitchoverPilot`, records the `DurableOperation` condition,
  and patches status before dropping the process-local driver
  (`kuberic-operator/src/reconciler.rs:1576-1597`).
- Explicit identity is deterministic from set identity, operation version,
  previous epoch, and target ID; the target snapshot changes only the
  configuration number and primary/role assignment
  (`kuberic-operator/src/durable/switchover.rs:32-104`).

#### Pilot admission

- A `durablePilot` request is rejected when the binary lacks the matching
  feature; there is no fallback to explicit execution
  (`kuberic-operator/src/reconciler.rs:234-252`).
- With the feature and runtime present, admission creates a random kernel
  execution identity, derives a deterministic ConfigMap name, embeds the exact
  initial operation, validates all pilot bounds, then writes
  `phase=Switchover`, clears `status.operation`, stores the pilot reference,
  and records `Accepted` before any checkpoint creation or effect
  (`kuberic-operator/src/durable/pilot.rs:1738-1819`,
  `kuberic-operator/src/reconciler.rs:1546-1575`).
- Repeated identical pilot requests have distinct execution, operation, and
  first-action identities, while a single persisted reference reconstructs
  the same execution contract (`kuberic-operator/src/durable/pilot.rs:2477-2505`).

#### Recovery routing

- `Phase::Switchover` selects the pilot only when `status.operation` is absent
  and `status.durableSwitchoverPilot` is present. All other switchover states
  enter the explicit durable-operation reconciler
  (`kuberic-operator/src/reconciler.rs:1788-1820`).
- The selector is not reread for recovery. Persisted status shape therefore
  determines which engine resumes
  (`kuberic-operator/src/reconciler.rs:1788-1820`).
- Explicit recovery reloads `status.operation`, rejects missing previous
  topology or incarnation drift before mutation, reconstructs fresh handles
  and observations, invokes the operation-specific decision function, and
  persists a transition or dispatch fence before the corresponding effect
  (`kuberic-operator/src/reconciler.rs:3343-3487`,
  `kuberic-operator/src/reconciler.rs:3488-3612`).
- Pilot recovery validates the reference and execution contract, obtains the
  execution-keyed shared host, constructs a fresh adapter from current Pods,
  and runs the shared bounded runner
  (`kuberic-operator/src/reconciler.rs:2822-2863`).

#### Publication

- Explicit completion refreshes the terminal snapshot with observed election
  metadata, updates epoch/current and target primary/phase/members/stable
  snapshot, retains the completed operation in `status.operation`, records
  the `DurableOperation` condition, and uses the committed-status persistence
  path (`kuberic-operator/src/reconciler.rs:3688-3759`).
- Pilot terminal state is persisted by the kernel before the runner returns a
  publication handoff. A `Stopped` terminal records a quarantined condition
  without publishing topology
  (`kuberic-operator/src/durable/runner.rs:404-427`,
  `kuberic-operator/src/reconciler.rs:2978-2997`).
- A successful or compensated pilot terminal rebuilds members from the
  terminal snapshot and persisted member records, updates the stable topology,
  clears `status.operation`, retains the pilot reference, writes the pilot
  terminal condition, persists committed status, logs measurements, then
  evicts only the process-local host
  (`kuberic-operator/src/reconciler.rs:2998-3099`).
- Terminal reload precedes operation-specific evidence collection in the
  shared runner, so retained terminal publication does not require workflow
  polling or replica observation
  (`kuberic-operator/src/durable/runner.rs:233-280`,
  `examples/kvstore/tests/reconciler.rs:4755-4863`).

### 3. Shared Switchover State Machine and Safety Sequence

- Admission validates a structurally valid stable snapshot, rejects the
  current primary and non-members as targets, checks configuration-number
  overflow, preserves exact member incarnations and quorum, and creates the
  target snapshot at the next configuration epoch
  (`kuberic-operator/src/durable/switchover.rs:32-104`).
- The forward sequence is:
  `Revoke → CaptureLsn → PreCatchUp → DemoteOldPrimary → PromoteTarget →
  DistributeEpoch → UpdateCatchUpConfiguration → WaitForCatchUpQuorum →
  UpdateCurrentConfiguration → LabelTargetPrimary → LabelOldSecondary →
  Finalize` (`kuberic-operator/src/durable/switchover.rs:106-340`,
  `kuberic-operator/src/durable/switchover.rs:449-477`).
- Frozen-LSN capture requires the exact old-primary incarnation at the
  previous epoch, still in primary role, with writes already
  `ReconfigurationPending`
  (`kuberic-operator/src/durable/switchover.rs:148-190`).
- Pre-catch-up requires the exact target incarnation at the previous epoch,
  in `ActiveSecondary`, with `current_progress >= frozen_lsn`; timeout moves
  to previous-configuration restoration instead of promotion
  (`kuberic-operator/src/durable/switchover.rs:191-240`).
- Every effect is represented by one deterministic pending action. Target
  selection, expected epoch, desired postcondition, deadline, and action ID
  are derived from the operation phase and snapshot
  (`kuberic-operator/src/durable/switchover.rs:965-1075`).
- Recovery checks the correlated agent ledger first, requires the exact action
  signature, and otherwise classifies live state as exact precondition,
  exact postcondition, in-progress, failed, unavailable, or impossible.
  Impossible state poisons the operation
  (`kuberic-operator/src/durable/switchover.rs:599-900`).
- The action mapping uses separate `DurableReplicaAction` values for revoke,
  demote/promote, epoch updates, CatchUp configuration, quorum wait, Current
  configuration, and restoration/compensation; routing-label actions remain
  Cluster API effects (`kuberic-operator/src/durable/switchover.rs:901-963`).
- Successful finalization attests exact member incarnations, epoch, roles,
  routing labels, and primary Current configuration before emitting a
  terminal snapshot (`kuberic-operator/src/durable/switchover.rs:324-340`,
  `kuberic-operator/src/durable/switchover.rs:1145-1182`).

### 4. Compensation Boundaries

- A target catch-up timeout transitions to
  `RestorePreviousConfiguration`; demotion failure also restores the previous
  Current configuration. Successful restoration reaches `Failed` and is
  terminally attested against the previous epoch/topology
  (`kuberic-operator/src/durable/switchover.rs:191-240`,
  `kuberic-operator/src/durable/switchover.rs:449-477`,
  `kuberic-operator/src/durable/switchover.rs:1086-1120`).
- A failed or uncertain target-promotion path transitions through
  `CompensatePromoteOldPrimary`, distributes the new epoch to all retained
  non-old-primary members, installs compensation CatchUp and Current
  configurations, restores the old-primary and target-secondary labels, and
  performs final attestation
  (`kuberic-operator/src/durable/switchover.rs:342-419`,
  `kuberic-operator/src/durable/switchover.rs:449-477`).
- Compensation terminal validation accepts the old primary only at either the
  exact previous epoch or the exact target epoch and requires all original
  member IDs/incarnations and roles to match the compensated topology
  (`kuberic-operator/src/durable/switchover.rs:106-139`,
  `kuberic-operator/src/durable/switchover.rs:1413-1480`).
- Timeouts after epoch/configuration convergence or routing-label work enter
  `Poisoned`; those paths do not manufacture a stable publication
  (`kuberic-operator/src/durable/switchover.rs:1086-1120`).

### 5. Pilot Contract, Replay, and Operation-Specific Adapter

- Current pilot contract version is 2. It declares at most 3 replicas, 32
  activity records, 64 workflow transitions, 3,000 workflow-input/operation
  bytes, 8,192 activity-input bytes, 4,096 activity-result bytes, 4,096
  terminal-payload bytes, and one 752 KiB encoded limit used for both active
  and terminal checkpoints (`kuberic-operator/src/durable/pilot.rs:68-76`,
  `kuberic-operator/src/durable/pilot.rs:1821-1828`).
- Workflow input contains version, execution ID, and the full immutable
  initial operation. Each activity contains only the mutable phase/frozen
  LSN/cursor/deadline/pending-action/error projection plus a passive,
  prepared-replica, or prepared-label kind
  (`kuberic-operator/src/durable/pilot.rs:155-224`).
- Activity results are `Advance`, one bounded `ProvenNoAdmission`,
  `Complete`, or `Stopped`; successful terminal records retain compact state,
  stable snapshot, compensation flag, and activity accounting
  (`kuberic-operator/src/durable/pilot.rs:226-324`).
- Workflow replay validates version and execution identity, advances
  effect-free transitions in memory, rejects no-progress results, limits one
  proven-non-admission redelivery per action identity, validates completion
  transitions and terminal topology, and stops after 64 transitions
  (`kuberic-operator/src/durable/pilot.rs:329-530`).
- `evaluate_adapter_step` directly invokes the explicit `decide` function and
  translates its `Persist`, `Wait`, exact external effect, completion, and
  poisoned results into pilot boundaries
  (`kuberic-operator/src/durable/pilot.rs:843-923`).
- The prepared resolver accepts a logical passive boundary, derives the exact
  replica or label command from current authoritative evidence, persists
  frozen dispatch evidence into compact state, and on replay validates and
  returns the exact recorded activity for byte comparison
  (`kuberic-operator/src/durable/pilot.rs:925-1130`).
- `SwitchoverRunnerAdapter` gathers exact-incarnation observations only after
  the runner has ruled out a stored terminal, maps shared permits to
  operation-specific effects, interprets quarantine, applies the pending or
  phase deadline, classifies unsupported envelope formats as incompatible,
  and validates the loaded terminal before publication handoff
  (`kuberic-operator/src/durable/pilot.rs:1142-1496`).

### 6. Shared Runner, Host, Store, and Provider Interfaces

#### Runner/adapter boundary

- `DurableOperationAdapter` exposes the operation-specific seams:
  `resolver`, optional `restore`, `prepare`, `observe_or_dispatch`,
  `resolve_quarantine`, deadline, preparation-wait policy, checkpoint
  disposition, terminal validation, and publication handoff
  (`kuberic-operator/src/durable/runner.rs:105-175`).
- `DurableRunner` performs authoritative load and validation before adapter
  preparation, short-circuits stored terminals, invokes fused turn/exposure,
  consumes at most its configured number of host outcomes, and maps adapter
  waits to one-to-ten-second deadline-clamped requeues
  (`kuberic-operator/src/durable/runner.rs:218-505`,
  `kuberic-operator/src/durable/runner.rs:599-604`).
- Conflict and unknown persistence outcomes return `ReloadRequired`; storage
  errors, checkpoint rejection/incompatibility/isolation, observation
  rejection, and nondeterminism remain distinct runner outcomes
  (`kuberic-operator/src/durable/runner.rs:191-212`,
  `kuberic-operator/src/durable/runner.rs:437-493`).

#### Host and exact exposure

- `PreparedActivityResolver` receives the logical request and the recorded
  exact request during replay; the kernel performs the final byte-for-byte
  match. `DurableActivity` owns immutable name/version and independent input
  and result bounds (`durable-execution/src/typed.rs:5-50`,
  `durable-execution/src/typed.rs:52-131`).
- The host's `DispatchPermit` constructor is private. A permit contains the
  exact logical activity and attempt ID
  (`durable-execution/src/host.rs:80-112`).
- `turn_and_expose_with` loads and validates authoritative state, returns a
  stored terminal without workflow polling, quarantines an already exposed
  final activity, and persists the exact prepared activity before returning a
  permit (`durable-execution/src/host.rs:326-465`).
- `observe_and_turn_with` validates exact activity identity and result size,
  then combines the observation with the next exposed activity or terminal in
  one compare-and-swap (`durable-execution/src/host.rs:540-686`).
- Exposure returns a permit only after accepted CAS. Conflict and unknown
  outcomes map to reload rather than a permit
  (`durable-execution/src/host.rs:710-767`,
  `durable-execution/src/host.rs:848-856`).
- `DurablePermitGuard` additionally enforces exact activity/spec/attempt
  matching and consumes the permit at most once
  (`kuberic-operator/src/durable/workflow_host.rs:20-62`).

#### Runtime, store, and ConfigMap provider

- `DurableWorkflowRuntime` keys hosts by namespace, set name, set UID,
  workflow name, and execution ID; a process epoch is shared across its host
  cache, while checkpoints remain authoritative
  (`kuberic-operator/src/durable/workflow_host.rs:64-155`).
- The current operator constructs one shared `DurableWorkflowRuntime` and
  passes it to both the feature-gated switchover runtime and production remove
  runtime (`kuberic-operator/src/reconciler.rs:121-133`).
- `DurableCheckpointStore` delegates the portable load/CAS contract to either
  Kubernetes or in-memory providers. `MeasuredDurableCheckpointStore` records
  load/write outcomes, active and terminal sizes, completed boundaries, and
  operation-provided external/passive classification
  (`kuberic-operator/src/durable/checkpoint_store.rs:18-113`,
  `kuberic-operator/src/durable/checkpoint_store.rs:186-278`,
  `kuberic-operator/src/durable/checkpoint_store.rs:280-387`).
- Kernel format 3 validates envelope version, exact execution ID and workflow
  input, terminal-payload bound, admitted active and terminal capacity,
  activity count, history shape, and result bounds
  (`durable-execution/src/checkpoint.rs:8-119`,
  `durable-execution/src/checkpoint.rs:127-306`,
  `durable-execution/src/checkpoint.rs:458-578`).
- Active checkpoints retain the full bounded activity vector. Terminal
  checkpoints retain only terminal outcome and completed activity count
  (`durable-execution/src/checkpoint.rs:154-238`).

### 7. Exact Command and Quarantine Mechanics

- `ReplicaEffectCommand` persists action ID and signature, target replica and
  incarnation, expected epoch and postcondition, expected agent generation
  and control version, observed runtime epoch, and exact encoded action
  payload (`kuberic-operator/src/durable/effects.rs:42-99`).
- Command preparation requires both the addressed handle and observed runtime
  to match the pending incarnation and requires correlated control protocol
  version 3. Previously frozen evidence is accepted only when generation,
  control version, runtime epoch, and action signature still match
  (`kuberic-operator/src/durable/effects.rs:204-330`).
- New dispatch evidence freezes generation, control version, runtime epoch,
  and the exact direct correlated action payload before exposure
  (`kuberic-operator/src/durable/effects.rs:405-470`).
- Replica dispatch reconstructs the action from the frozen payload, checks its
  signature, and sends a correlated request with protocol version, action ID,
  exact target identity, generation, control version, runtime epoch, and
  action (`kuberic-operator/src/durable/effects.rs:542-632`).
- The agent checks protocol, action ID, exact replica/incarnation, generation,
  signature, duplicate/conflicting identity, control-version continuity,
  single-active-work admission, and runtime epoch before accepting a local
  effect (`kuberic-core/src/replica_agent.rs:548-667`).
- Exact duplicate action IDs with the same signature replay the current or
  retained observation; reuse with different input is rejected
  (`kuberic-core/src/replica_agent.rs:596-617`).
- `LabelEffectCommand` binds replica ID, Pod name, expected UID, role, and a
  deterministic identity signature; execution uses
  `patch_pod_labels_if_uid`
  (`kuberic-operator/src/durable/effects.rs:102-147`,
  `kuberic-operator/src/durable/effects.rs:572-583`).
- Dispatch failures are separated into proven non-admission, definite
  failure, and unknown. Agent busy and precondition/continuity rejection are
  non-admission; conflict and typed terminal failure are definite; transport
  ambiguity remains unknown
  (`kuberic-operator/src/durable/effects.rs:655-704`).
- A permitted pilot replica command dispatches once. Success or unknown
  outcome becomes exposed; proven non-admission records one bounded
  redelivery result; conflict becomes a stopped fail-closed result. Label
  mutation always becomes exposed and awaits observation
  (`kuberic-operator/src/durable/effects.rs:946-1059`).
- Quarantine never owns a permit. It advances only from an exact correlated
  observation or exact live postcondition. A changed process generation plus
  absence of the old action record proves bounded non-admission; same
  generation, in-progress, mixed, unavailable, and unresolved label evidence
  remain awaiting evidence
  (`kuberic-operator/src/durable/effects.rs:714-793`,
  `kuberic-operator/src/durable/effects.rs:1061-1138`,
  `kuberic-operator/src/reconciler.rs:4979-5195`).

### 8. Legacy and Incompatible-State Precedents

#### Current switchover state

- Explicit switchover uses durable operation version 1, which is also the
  current accepted explicit version
  (`kuberic-operator/src/crd.rs:317-319`,
  `kuberic-operator/src/durable/switchover.rs:1184-1205`).
- Current pilot references and workflow input use version 2; malformed
  execution IDs, non-v2 references, changed deterministic checkpoint names,
  changed immutable workflow input, invalid terminal shape, and unsupported
  activity versions are rejected by reference, workflow, resolver, kernel, or
  terminal validation
  (`kuberic-operator/src/durable/pilot.rs:329-363`,
  `kuberic-operator/src/durable/pilot.rs:581-593`,
  `kuberic-operator/src/durable/pilot.rs:795-841`,
  `kuberic-operator/src/durable/pilot.rs:1770-1812`).
- Unsupported checkpoint envelope format is the only current pilot
  checkpoint error classified as `Incompatible`; other checkpoint validation
  failures are `Rejected`
  (`kuberic-operator/src/durable/pilot.rs:1471-1483`).
- Historical commit `a98ffb3` defined pilot contract version 1 and activity
  name `kuberic.switchover.explicit-step`
  (`a98ffb3:kuberic-operator/src/durable/pilot.rs:37-47`).
  Historical commit `3f1d9ad` defined pilot contract version 2 and
  `kuberic.switchover.effect-boundary`
  (`3f1d9ad:kuberic-operator/src/durable/pilot.rs:43-52`).
- The current pilot intentionally accepts an absent checkpoint after its
  reference is persisted: the shared runner treats `load(None)` as a new
  execution, the switchover adapter has no custom `restore` override, and
  admission states that the reference precedes checkpoint creation
  (`kuberic-operator/src/durable/runner.rs:233-296`,
  `kuberic-operator/src/durable/pilot.rs:1349-1496`,
  `kuberic-operator/src/reconciler.rs:1546-1575`). This is the concrete
  ordering constraint behind the specification's distinction between a
  newly admitted missing checkpoint and missing historical execution state.

#### Framework-native remove precedent

- The read-only prior research artifact identifies the shared-runner seams,
  compatibility categories, checkpoint ownership, measurement surfaces, and
  test traceability that were subsequently implemented for remove-replica
  (`.paw/work/framework-native-remove-replica/CodeResearch.md:321-405`,
  `.paw/work/framework-native-remove-replica/CodeResearch.md:444-559`).
- `KubericSetStatus` accepts unknown legacy fields for one reconciliation,
  omits them from replacement status, and preserves unknown status schema
  fields in the CRD (`kuberic-operator/src/crd.rs:130-139`,
  `kuberic-operator/src/crd.rs:1160-1190`).
- Before ordinary reconciliation, legacy remove pilot JSON or an explicit
  remove operation is atomically replaced by a typed native marker containing
  source, legacy version, identity, deterministic checkpoint reference, and
  stable fingerprint; the condition records that it cannot resume
  (`kuberic-operator/src/reconciler.rs:290-399`,
  `kuberic-operator/src/reconciler.rs:647-657`).
- Native remove references distinguish immutable admission from
  incompatibility evidence and reject markers, unsupported contract versions,
  malformed execution identities, and changed checkpoint names before host
  creation (`kuberic-operator/src/crd.rs:217-245`,
  `kuberic-operator/src/durable/remove_replica_execution.rs:1720-1813`).
- Native remove also has a typed `IncompatibleContract` terminal and maps
  unsupported envelope or immutable-capacity changes to the runner's
  incompatible disposition
  (`kuberic-operator/src/durable/remove_replica_execution.rs:276-309`,
  `kuberic-operator/src/durable/remove_replica_execution.rs:2359-2409`,
  `kuberic-operator/src/durable/remove_replica_execution.rs:2948-2984`).
- The implemented remove protocol document records the clean-break behavior:
  legacy pilot, legacy explicit, and non-v3 native state is never resumed,
  cleared as absent, or used to authorize replacement execution
  (`docs/features/kuberic/implemented/agent-owned-replica-remove-protocol.md:73-130`).

### 9. Evidence for Retaining Independently Correlated Local Actions

The current implementation evidence aligns with the specification decision to
retain independent local actions rather than introduce one coarse switchover
intent:

1. **The global sequence already has one durable authority.** The switchover
   operation owns phase, exact snapshots, old/target identities, frozen LSN,
   one pending action, deadlines, and compensation state
   (`kuberic-operator/src/crd.rs:322-391`). The pilot replays this same
   authority and only asks the adapter for one exact boundary at a time
   (`kuberic-operator/src/durable/pilot.rs:329-530`,
   `kuberic-operator/src/durable/pilot.rs:843-923`).
2. **Every replica mutation already has independent identity and evidence.**
   Pending actions carry deterministic action IDs, exact target incarnation,
   expected epoch/postcondition, and dispatch fences
   (`kuberic-operator/src/crd.rs:794-819`,
   `kuberic-operator/src/durable/switchover.rs:965-1075`).
   Agent admission and duplicate replay operate on that one action identity
   (`kuberic-core/src/replica_agent.rs:548-667`).
3. **The sequence crosses multiple authorities.** The success path addresses
   the old primary, target primary, every retained third member, and two
   Kubernetes Pod-label objects
   (`kuberic-operator/src/durable/switchover.rs:901-1075`,
   `kuberic-operator/src/durable/pilot.rs:2052-2073`). Compensation addresses
   the old primary plus all non-old-primary retained members and both routing
   labels (`kuberic-operator/src/durable/switchover.rs:342-419`,
   `kuberic-operator/src/durable/pilot.rs:2286-2308`).
4. **The current agent API has no switchover coordinator contract.**
   `DurableReplicaAction` and the protobuf oneof expose coarse add/remove
   intents plus the existing individual switchover primitives, but no
   switchover intent (`kuberic-core/src/types.rs:448-489`,
   `kuberic-core/proto/kuberic.proto:251-271`).
5. **The peer protocol has no switchover operation or stage.** It is limited
   to `AddBuild` and `Remove`, with Prepare/Activate/Cleanup/Retire stages
   (`kuberic-core/src/replica_lifecycle.rs:125-180`,
   `kuberic-core/proto/kuberic.proto:461-472`).
6. **Existing coarse intents are operation-specific rather than generic.**
   Add/build and remove retain separate intent, coordinator, configuration,
   commit, compensation, progress, and terminal contracts even though they
   share correlated control and lifecycle-peer transport
   (`docs/features/kuberic/implemented/agent-owned-replica-add-build-protocol.md:28-63`,
   `docs/features/kuberic/implemented/agent-owned-replica-remove-protocol.md:27-73`).
7. **Local agent evidence is bounded and process-local.** The agent retains
   one current action and 16 terminal actions per generation; absence of a
   retained record is not global non-execution evidence
   (`kuberic-core/src/replica_agent.rs:38-41`,
   `kuberic-core/src/replica_agent.rs:1715-1743`,
   `docs/features/kuberic/status.md:40-62`).
8. **Repository documentation describes the present ownership split.**
   Failover and switchover issue ordered correlated actions from the operator
   checkpoint, while add and remove place their local runtime sequence behind
   coarse primary-agent actions
   (`docs/features/kuberic/protocols.md:724-732`).

These facts establish the current constraints: a coarse switchover intent
would be a new operation-specific protocol and coordinator spanning multiple
replica agents, compensation epochs/configurations, and Kubernetes label
effects. No such action, peer operation, progress schema, terminal result, or
test surface exists in the current tree. Retaining the independent correlated
commands keeps the existing global checkpoint, per-command observation rules,
and exact-UID Kubernetes effects as the implemented ownership boundary.

### 10. Bounds, Projections, and Measurements

| Dimension | Current switchover pilot |
|---|---|
| Contract/reference version | 2 |
| Maximum replicas | 3 |
| Activity records | 32 |
| Workflow transition fuel | 64 |
| Workflow input / operation | 3,000 bytes |
| Activity input | 8,192 bytes |
| Activity result | 4,096 bytes |
| Active encoded checkpoint | 752 KiB |
| Terminal encoded checkpoint | Same 752 KiB limit |
| Terminal payload | 4,096 bytes |
| Error text | 512 characters |

The constants and shared active/terminal checkpoint limit are defined at
`kuberic-operator/src/durable/pilot.rs:68-76` and
`kuberic-operator/src/durable/pilot.rs:1821-1828`; terminal error text is
bounded at `kuberic-operator/src/durable/pilot.rs:1709-1736`.

- Admission computes maximum success/rollback activity counts, adds pure
  transition counts, projects a maximum active checkpoint, and rejects any
  projection above the declared limits
  (`kuberic-operator/src/durable/pilot.rs:1854-1908`).
- For three members, the success transcript is nine external effects and
  three passive observations; seven ReplicaAgent effects each have one
  redelivery slot, giving a 19-record maximum
  (`kuberic-operator/src/durable/pilot.rs:1971-2073`).
- The longest rollback transcript includes failed/uncertain target promotion,
  old-primary re-promotion, retained-member epoch convergence, compensation
  configurations, two label boundaries that may resolve externally or
  passively, and final attestation. Eight replica-effect redelivery slots give
  a 21-record maximum (`kuberic-operator/src/durable/pilot.rs:2189-2211`,
  `kuberic-operator/src/durable/pilot.rs:2286-2308`).
- Terminal accounting validates exact reachable external/passive pairs
  against the terminal phase and completed activity count
  (`kuberic-operator/src/durable/pilot.rs:2104-2187`,
  `kuberic-operator/src/durable/pilot.rs:3810-3982`).
- The maximum checkpoint fixture fills all 32 records with maximum-sized
  inputs and results and uses the full workflow-input and terminal-payload
  declarations (`kuberic-operator/src/durable/pilot.rs:2317-2356`).
- The current maximum-bound test proves the shared encoded ceiling and one
  byte below that ceiling, while separate tests cover the replica cap,
  workflow input, result encoding, and terminal encoding
  (`kuberic-operator/src/durable/pilot.rs:2523-2588`,
  `kuberic-operator/src/durable/pilot.rs:3457-3535`).
- The shared measurement store separately records accepted writes, active
  bytes, terminal bytes, completed activities, external effects, and passive
  observations (`kuberic-operator/src/durable/checkpoint_store.rs:95-113`,
  `kuberic-operator/src/durable/checkpoint_store.rs:303-365`).
- The canonical unit runner measurement asserts 9 external effects, 3 passive
  observations, 12 completed activities, and 13 accepted writes
  (`kuberic-operator/src/durable/pilot.rs:4428-4503`).
- The integration measurement additionally derives exact terminal-payload
  bytes and checks the payload bound, but its printed record includes active
  and terminal checkpoint bytes without printing that payload-byte value
  (`examples/kvstore/tests/reconciler.rs:3705-3816`).
- The testing guide records six observed maximum active sizes of
  31,597-31,605 bytes, terminal sizes of 4,077-4,081 bytes, and identifies the
  752 KiB and 4,096-byte values as stable admission limits rather than exact
  measured compatibility constants (`docs/features/kuberic/testing.md:410-446`).
- The framework-native remove precedent separates active and terminal
  checkpoint bounds and has exact one-unit-over tests for record count, input,
  result, active record, terminal record, and terminal payload
  (`kuberic-operator/src/durable/remove_replica_execution.rs:71-83`,
  `kuberic-operator/src/durable/remove_replica_execution.rs:4786-4863`).

### 11. Test Inventory

#### Explicit-path regression coverage

| Behavior | Test evidence |
|---|---|
| End-to-end switchover, stable topology, new-primary writes, old-primary rejection | `test_reconciler_switchover` (`examples/kvstore/tests/reconciler.rs:3427-3628`) |
| Reconciler state loss after every boundary | `test_durable_switchover_survives_state_loss_at_every_boundary` (`examples/kvstore/tests/reconciler.rs:5190-5278`) |
| Lost target-promotion reply without duplicate dispatch | `test_durable_switchover_observes_lost_promotion_reply_without_duplicate` (`examples/kvstore/tests/reconciler.rs:5280-5357`) |
| Lost replies across all seven replica mutation kinds | `test_durable_switchover_observes_every_runtime_lost_reply_window` (`examples/kvstore/tests/reconciler.rs:5359-5495`) |
| Failed-promotion compensation | `test_durable_switchover_compensates_failed_target_promotion` (`examples/kvstore/tests/reconciler.rs:5497-5587`) |
| Pre-promotion restoration of writes and previous epoch | `test_durable_switchover_restores_writes_when_demotion_never_runs` (`examples/kvstore/tests/reconciler.rs:5589-5676`) |
| Status conflict and unsupported operation version do not mutate | `test_durable_switchover_conflict_and_invalid_checkpoint_do_not_mutate` (`examples/kvstore/tests/reconciler.rs:5678-5752`) |
| Incarnation drift before mutation | `test_durable_switchover_rejects_incarnation_drift_before_mutation` (`examples/kvstore/tests/reconciler.rs:5754-5820`) |

#### Pilot unit and shared-runner coverage

| Behavior | Test evidence |
|---|---|
| Distinct request identity and deterministic reference replay | `references_are_stable_but_repeated_requests_are_distinct` (`kuberic-operator/src/durable/pilot.rs:2477-2505`) |
| Owner scope and owner flags | `owner_is_same_namespace_non_controlling_and_non_blocking` (`kuberic-operator/src/durable/pilot.rs:2507-2521`) |
| Replica admission cap and maximum encoded projection | `admission_rejects_more_than_three_members`, `maximum_projected_history_fits_both_budgets` (`kuberic-operator/src/durable/pilot.rs:2523-2555`) |
| Exposed activity quarantine after host restart | `exposed_activity_is_quarantined_after_host_restart` (`kuberic-operator/src/durable/pilot.rs:2786-2815`) |
| Exact one-use permit | `permit_guard_consumes_matching_permit_once` (`kuberic-operator/src/durable/pilot.rs:2867-2925`) |
| Every dispatch fence participates in exact activity identity | `prepared_replica_spec_binds_every_dispatch_fence` (`kuberic-operator/src/durable/pilot.rs:2927-3001`) |
| Semantic drift, activity identity drift, and exact replay bytes | `restart_validation_rejects_consistently_rewritten_role_and_epoch_payloads`, `replay_rejects_recorded_command_semantic_drift_as_activity_mismatch`, `replay_rejects_changed_activity_name_version_and_result_bound`, and `legacy_activity_v1_without_kind_replays_with_exact_recorded_bytes` (`kuberic-operator/src/durable/pilot.rs:3003-3304`) |
| One proven-non-admission redelivery per action | `proven_no_admission_redelivery_is_bounded_per_action` (`kuberic-operator/src/durable/pilot.rs:3351-3404`) |
| Compensation terminal and loaded-terminal revalidation | `compensation_branch_terminalizes_failed_old_primary_topology`, `loaded_terminal_revalidates_identity_topology_and_outcome_kind` (`kuberic-operator/src/durable/pilot.rs:3406-3455`, `kuberic-operator/src/durable/pilot.rs:3605-3808`) |
| Reachable terminal-accounting pairs | `terminal_accounting_matches_exact_reachable_projection_pairs` (`kuberic-operator/src/durable/pilot.rs:3810-3982`) |
| Unknown/conflict persistence outcomes reload without false permits | `schedule_unknown_outcomes_reload_before_any_permit`, `observation_faults_reload_effect_free_steps_without_dispatch`, `exposure_faults_reload_without_false_permits`, and `schedule_conflict_reloads_before_any_permit` (`kuberic-operator/src/durable/pilot.rs:4057-4337`) |
| Exact 9/3/12/13 accounting and 19/21 projections | `measurements_switchover_runner_preserves_happy_path_accounting`, `success_and_rollback_transcripts_fit_with_redelivery_headroom` (`kuberic-operator/src/durable/pilot.rs:4428-4537`) |

#### Pilot reconciler coverage

| Behavior | Test evidence |
|---|---|
| Happy path, exact mutation order, 9/3/12/13 accounting, terminal retention, later explicit request | `test_durable_execution_switchover_pilot_happy_path` (`examples/kvstore/tests/reconciler.rs:3630-3873`) |
| Active, incompatible, rejected, isolated, conflict, unknown-write, persistence failure, nondeterministic, and terminal outcomes | `test_durable_execution_switchover_pilot_fr017_operation_outcome_matrix` (`examples/kvstore/tests/reconciler.rs:3877-4280`) |
| Fresh-fence bounded redelivery | `test_durable_execution_switchover_pilot_fr019_deadline_policy_preserves_fresh_fence_redelivery` (`examples/kvstore/tests/reconciler.rs:4282-4416`) |
| Restart after every turn | `test_durable_execution_switchover_pilot_fr019_observation_collection_survives_restart_every_turn` (`examples/kvstore/tests/reconciler.rs:4418-4500`) |
| Failed-promotion compensation and accounting | `test_durable_execution_switchover_pilot_fr019_publication_compensates_failed_promotion` (`examples/kvstore/tests/reconciler.rs:4502-4601`) |
| Lost promotion reply and all lost-reply windows | `test_durable_execution_switchover_pilot_observes_lost_promotion_reply_once`, `test_durable_execution_switchover_pilot_fr019_exact_effect_dispatch_observes_every_lost_reply_once` (`examples/kvstore/tests/reconciler.rs:4603-4753`) |
| Terminal reload before publication and terminal CAS conflict | `test_durable_execution_switchover_pilot_fr019_terminal_validation_reloads_before_publication`, `test_durable_execution_switchover_pilot_reloads_after_terminal_cas_conflict` (`examples/kvstore/tests/reconciler.rs:4755-4949`) |
| Stale target incarnation and unknown checkpoint outcomes | `test_durable_execution_switchover_pilot_fr019_authority_preparation_rejects_stale_target_incarnation`, `test_durable_execution_switchover_pilot_unknown_checkpoint_outcomes_requeue_without_effect` (`examples/kvstore/tests/reconciler.rs:4951-5127`) |

#### Exact-effect bridge coverage

- Unit tests prove a fused permit dispatches one exact persisted command and
  cannot be consumed twice
  (`kuberic-operator/src/reconciler.rs:5373-5503`).
- Missing observations reject preparation without checkpoint creation or
  dispatch, then fresh evidence permits one exact command
  (`kuberic-operator/src/reconciler.rs:5505-5654`).
- Precondition rejection clears stale fences, persists non-admission, waits
  for a fresh observation cycle, and only then exposes the bounded
  redelivery (`kuberic-operator/src/reconciler.rs:5656-5820`).
- All seven ReplicaAgent switchover commands traverse fused exposure and the
  one-use permitted bridge (`kuberic-operator/src/reconciler.rs:5822-5965`).
- Exact snapshot UID label commands and label-quarantine behavior are covered
  by the switchover bridge tests
  (`kuberic-operator/src/reconciler.rs:5050-5327`).

### 12. CRD, Generated Manifest, CI, Examples, and Documentation

- The CRD source is derived with `kube::CustomResource`, including status and
  printer columns (`kuberic-operator/src/crd.rs:1-25`).
- The checked-in deployment manifest is validated by source tests for
  switchover selector/status presence, unknown-field preservation, and
  least-privilege ConfigMap writer verbs
  (`kuberic-operator/src/crd.rs:1082-1117`,
  `kuberic-operator/src/crd.rs:1180-1210`).
- The generated/deployed old split is visible in
  `spec.switchoverExecutionMode`, `status.durableSwitchoverPilot`, and the
  shared `status.operation` schema
  (`kuberic-operator/deploy/deployment.yaml:96-114`,
  `kuberic-operator/deploy/deployment.yaml:149-175`,
  `kuberic-operator/deploy/deployment.yaml:235-260`).
- The ordinary example does not opt into the pilot
  (`examples/kvstore/deploy/kubericset.yaml:1-14`).
- CI's switchover-specific gate still names and enables the pilot, while the
  later build/image/full-test steps are common repository gates
  (`.github/workflows/CI.yml:52-60`,
  `.github/workflows/CI.yml:73-86`).
- `kuberic-operator/README.md` documents the explicit default, pilot selector,
  build feature, ConfigMap storage, pilot status/condition, and measurements
  (`kuberic-operator/README.md:43-90`).
- `docs/features/kuberic/operator.md` documents explicit CRD status recovery,
  the shared runner/provider, optional pilot selection, exact-command
  exposure, quarantine, terminal-before-publication, and ConfigMap ownership
  (`docs/features/kuberic/operator.md:200-270`,
  `docs/features/kuberic/operator.md:290-342`).
- `docs/features/kuberic/protocols.md` documents the ordered explicit
  switchover sequence, bounded local ledger, feature-gated replay path, and
  the present fine-grained operator ownership boundary
  (`docs/features/kuberic/protocols.md:118-166`,
  `docs/features/kuberic/protocols.md:724-732`).
- `docs/features/kuberic/testing.md` documents the explicit recovery baseline,
  pilot ambiguity matrix, commands, 9/3/12/13 accounting, byte measurements,
  configured bounds, and provider validation
  (`docs/features/kuberic/testing.md:381-486`).
- `docs/features/kuberic/durable-execution-roadmap.md` describes the kernel,
  shared runner/provider boundary, optional switchover consumer, and
  operation-specific limits
  (`docs/features/kuberic/durable-execution-roadmap.md:1-50`,
  `docs/features/kuberic/durable-execution-roadmap.md:160-184`).
- `docs/features/kuberic/status.md` records explicit CRD-backed switchover as
  implemented and agent-owned switchover as an open candidate
  (`docs/features/kuberic/status.md:24-35`,
  `docs/features/kuberic/status.md:163-168`).
- `docs/features/kuberic/design-gaps.md` records write revocation,
  post-revoke target catch-up, compensation, exact ordering, and durable
  restart recovery as implemented behavior
  (`docs/features/kuberic/design-gaps.md:43-135`,
  `docs/features/kuberic/design-gaps.md:717-861`,
  `docs/features/kuberic/design-gaps.md:901-930`).
- `docs/features/kuberic/rolling-upgrade-design.md` composes future primary
  upgrade behavior with the existing durable switchover and frozen-LSN
  boundary (`docs/features/kuberic/rolling-upgrade-design.md:515-518`,
  `docs/features/kuberic/rolling-upgrade-design.md:802-812`).
- Root `README.md` contains only the general graceful-switchover capability
  and links to the protocol/operator documentation; it does not expose the
  engine selector (`README.md:5-12`, `README.md:65-72`).

## Architecture Documentation

1. **Reconciliation is the scheduler.** The kube controller invokes
   `reconcile_set`; the durable runner is called inline and returns a bounded
   requeue or terminal handoff. No pilot worker, queue, lease, watcher, or
   separate durable service exists
   (`kuberic-operator/src/main.rs:29-130`,
   `kuberic-operator/src/durable/runner.rs:218-505`,
   `kuberic-operator/src/durable/effects.rs:1-6`).
2. **Global and local authority are separate.** CRD status or the referenced
   ConfigMap is the durable global workflow authority. The ReplicaAgent owns
   one generation-qualified local action plus a bounded terminal ledger
   (`kuberic-operator/src/crd.rs:115-139`,
   `kuberic-core/src/replica_agent.rs:38-41`,
   `kuberic-core/src/replica_agent.rs:1715-1743`).
3. **The framework persists exact external boundaries.** Logical workflow
   requests are resolved against live authority into exact prepared command
   bytes; replay validates recorded semantics and the kernel performs exact
   matching before exposure (`durable-execution/src/typed.rs:27-50`,
   `kuberic-operator/src/durable/pilot.rs:925-1130`).
4. **Publication is outside the runner.** The runner validates and returns an
   operation-specific terminal; the reconciler owns CRD topology/status
   publication and process-local host eviction
   (`kuberic-operator/src/durable/runner.rs:105-175`,
   `kuberic-operator/src/reconciler.rs:2959-3099`).
5. **Compatibility is layered.** Kernel envelope version, immutable execution
   contract, workflow/reference version, activity version, typed terminal
   shape, CRD status shape, and deterministic provider identity are validated
   at separate boundaries
   (`durable-execution/src/checkpoint.rs:8-145`,
   `kuberic-operator/src/durable/pilot.rs:329-363`,
   `kuberic-operator/src/durable/pilot.rs:581-593`,
   `kuberic-operator/src/durable/pilot.rs:795-841`,
   `kuberic-operator/src/durable/pilot.rs:1770-1819`).
6. **Framework-native remove is the current graduation precedent.** It uses
   one non-feature-gated status reference, a separately versioned compact
   contract, independent active/terminal limits, typed incompatibility, the
   same shared runtime/runner/provider, and adapter-owned operation semantics
   (`kuberic-operator/src/durable/remove_replica_execution.rs:71-126`,
   `kuberic-operator/src/durable/remove_replica_execution.rs:1621-1695`,
   `kuberic-operator/src/durable/remove_replica_execution.rs:2672-2988`).

## Code References

- `kuberic-operator/src/crd.rs:46-56` — public switchover engine selector.
- `kuberic-operator/src/crd.rs:115-139` — duplicate explicit/pilot status
  surfaces.
- `kuberic-operator/src/crd.rs:194-215` — selector enum and pilot reference.
- `kuberic-operator/src/crd.rs:322-391` — explicit operation state.
- `kuberic-operator/src/crd.rs:794-819` — independent pending-action identity
  and dispatch evidence.
- `kuberic-operator/src/reconciler.rs:1521-1597` — split admission.
- `kuberic-operator/src/reconciler.rs:1788-1820` — status-based recovery
  routing.
- `kuberic-operator/src/reconciler.rs:2822-3100` — pilot runner and
  publication.
- `kuberic-operator/src/reconciler.rs:3343-3759` — explicit recovery,
  dispatch, and publication.
- `kuberic-operator/src/durable/switchover.rs:32-598` — operation creation,
  sequence, compensation, and legal transitions.
- `kuberic-operator/src/durable/switchover.rs:599-1120` — observation,
  action mapping, exact pending identities, and timeout behavior.
- `kuberic-operator/src/durable/switchover.rs:1145-1480` — final topology and
  terminal validation.
- `kuberic-operator/src/durable/pilot.rs:68-76` — current pilot bounds.
- `kuberic-operator/src/durable/pilot.rs:155-530` — typed contract and replay.
- `kuberic-operator/src/durable/pilot.rs:620-1130` — prepared command and
  replay validation.
- `kuberic-operator/src/durable/pilot.rs:1142-1496` — switchover runner
  adapter.
- `kuberic-operator/src/durable/pilot.rs:1738-1908` — admission, identity,
  storage ownership, and projections.
- `kuberic-operator/src/durable/pilot.rs:1911-2356` — success/rollback
  projections and checkpoint sizing.
- `kuberic-operator/src/durable/effects.rs:42-147` — exact replica and label
  commands.
- `kuberic-operator/src/durable/effects.rs:204-330` — command preparation.
- `kuberic-operator/src/durable/effects.rs:542-704` — correlated dispatch and
  outcome classification.
- `kuberic-operator/src/durable/effects.rs:714-793` — shared quarantine
  resolution.
- `kuberic-operator/src/durable/effects.rs:946-1138` — switchover effect bridge
  and quarantine interpretation.
- `kuberic-operator/src/durable/runner.rs:105-175` — shared adapter interface.
- `kuberic-operator/src/durable/runner.rs:218-505` — shared bounded lifecycle
  loop.
- `kuberic-operator/src/durable/workflow_host.rs:20-185` — one-use permit,
  host cache, provider construction, and measurement retention.
- `kuberic-operator/src/durable/checkpoint_store.rs:18-113` — store and
  measurement abstractions.
- `durable-execution/src/host.rs:326-767` — fused exposure, quarantine, fused
  observation/progression, and terminal persistence.
- `durable-execution/src/checkpoint.rs:8-306` — format, execution contract,
  active/terminal state, and independent lifecycle limits.
- `durable-execution/src/kubernetes.rs:242-476` — ConfigMap identity, contents,
  CAS, owner matching, and owner validation.
- `kuberic-core/src/types.rs:448-489` — current coarse intents and individual
  switchover actions.
- `kuberic-core/src/replica_agent.rs:548-667` — correlated action admission and
  duplicate replay.
- `kuberic-core/src/replica_lifecycle.rs:125-180` — add/remove-only peer
  operation and stage vocabulary.
- `kuberic-core/proto/kuberic.proto:73-86` — operator-facing correlated
  control service.
- `kuberic-core/proto/kuberic.proto:251-271` — wire action oneof.
- `kuberic-core/proto/kuberic.proto:461-472` — wire peer operation/stage
  vocabulary.

## Open Questions

- The repository cannot establish whether any deployed cluster currently
  retains explicit v1 or pilot v1/v2 switchover state.
- The current integration measurement calculates and bounds exact terminal
  payload bytes but does not print the value
  (`examples/kvstore/tests/reconciler.rs:3714-3723`,
  `examples/kvstore/tests/reconciler.rs:3763-3816`).
- A separate framework-native compact switchover contract and its independent
  production bound values do not exist at this research commit; the available
  numerical evidence is the current pilot contract/projections and the
  framework-native remove precedent
  (`kuberic-operator/src/durable/pilot.rs:68-76`,
  `kuberic-operator/src/durable/remove_replica_execution.rs:71-83`).
- No user input is required to proceed from this implementation map.
