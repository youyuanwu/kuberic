---
date: 2026-09-07T20:38:46.555+00:00
git_commit: 7daf612682e98044542cee07fb0de82b16fbb808
branch: feature/framework-native-remove-replica
repository: github.com/youyuanwu/kuberic
topic: "Framework-Native Remove Replica"
tags: [research, codebase, durable-execution, remove-replica, operator]
status: complete
last_updated: 2026-09-07
historical_snapshot: true
---

# Research: Framework-Native Remove Replica

> **Historical PAW research snapshot:** Findings describe the pre-migration
> tree at the commit recorded above. They are retained as planning evidence,
> not as current product documentation.

## Research Question

Where are remove-replica execution, recovery, persistence, safety checks,
measurements, tests, and public pilot surfaces implemented, and which existing
seams define the common durable runner, compact contract, compatibility, and
validation boundaries required by the approved specification?

## Summary

The current tree has two operator-hosted remove paths. Acceptance defaults to
an explicit `status.operation` state machine and optionally creates a
`status.durableRemoveReplicaPilot` reference when both the resource selector
and Cargo feature choose the pilot (`kuberic-operator/src/crd.rs:54-63`,
`kuberic-operator/src/reconciler.rs:265-356`). Recovery routing later chooses
between those persisted status shapes (`kuberic-operator/src/reconciler.rs:1797-1835`).
Both paths call the same remove decision implementation:
`start_remove_replica` freezes admission authority and
`decide_remove_replica` advances the operation
(`kuberic-operator/src/durable/remove_replica.rs:42-148`,
`kuberic-operator/src/durable/remove_replica.rs:150-231`).

The durable kernel already provides the common persistence vocabulary:
versioned bounded activities, prepared-activity replay validation, private
dispatch permits, active/terminal checkpoint states, fused
observation/progression, quarantine, reload on conflict or unknown outcome,
and terminal compaction (`durable-execution/src/typed.rs:27-50`,
`durable-execution/src/typed.rs:52-131`,
`durable-execution/src/host.rs:95-164`,
`durable-execution/src/host.rs:311-445`,
`durable-execution/src/host.rs:540-686`,
`durable-execution/src/checkpoint.rs:10-119`,
`durable-execution/src/checkpoint.rs:154-273`). The operator already shares a
workflow-keyed host cache, a one-use permit guard, provider construction, and
workflow-neutral measurement hooks
(`kuberic-operator/src/durable/workflow_host.rs:18-155`,
`kuberic-operator/src/durable/pilot_store.rs:14-93`).

The remaining lifecycle loop is duplicated in the reconciler: switchover owns
one complete host-outcome loop at
`kuberic-operator/src/reconciler.rs:2834-3203`, and durable remove owns a
parallel loop at `kuberic-operator/src/reconciler.rs:3481-3912`. Their common
shape is load, terminal short-circuit, reconstruct state, collect evidence,
prepare the next exact boundary, process permit/quarantine/reload/failure, and
publish a validated terminal. Their operation-specific portions are
observation collection, prepared-command resolution, effect dispatch and
quarantine interpretation, deadline policy, terminal validation, condition
text, and final publication.

The present remove contract repeats the complete mutable projection in every
activity input and every successful result
(`kuberic-operator/src/durable/remove_replica_pilot.rs:135-190`,
`kuberic-operator/src/durable/remove_replica_pilot.rs:191-302`). The immutable
initial operation is also serialized in the CRD reference and again in the
workflow input (`kuberic-operator/src/durable/remove_replica_pilot.rs:1415-1499`).
The active checkpoint retains every completed input and result until
terminalization (`durable-execution/src/checkpoint.rs:154-214`,
`docs/features/kuberic/durable-execution-roadmap.md:45-50`). These are the
implementation locations corresponding to the measured active-record growth
documented in the current roadmap and test guide
(`docs/features/kuberic/durable-execution-roadmap.md:234-260`,
`docs/features/kuberic/testing.md:526-547`).

## Documentation System

- **Framework**: Plain checked-in Markdown. No MkDocs, Docusaurus, Sphinx,
  mdBook, or navigation configuration is present.
- **Docs Directory**: `docs/`, with feature documentation under
  `docs/features/kuberic/` and implemented protocol decisions under
  `docs/features/kuberic/implemented/` (`README.md:65-72`).
- **Navigation Config**: N/A; the root README is the manual index
  (`README.md:65-72`).
- **Style Conventions**: Design documents use descriptive headings, bullet
  lists, tables, fenced command examples, and explicit protocol/decision
  sections (`docs/features/kuberic/durable-execution-roadmap.md:1-50`,
  `docs/features/kuberic/implemented/agent-owned-replica-remove-protocol.md:40-85`).
- **Build Command**: N/A for prose. Rust documentation tests use
  `cargo test --doc --workspace` (`docs/features/kuberic/testing.md:200-220`).
- **Standard Files**: `README.md`, `kuberic-operator/README.md`,
  `durable-execution/README.md`, `docs/Dev.md`, and `docs/Internals.md`.
  No `CHANGELOG.md` or `CONTRIBUTING.md` is present.

## Verification Commands

- **Test Command**:
  `cargo test -p kuberic-core -p kuberic-operator -p kvstore -p sqlite-replicated`
  (`docs/features/kuberic/testing.md:200-205`).
- **Documentation Tests**: `cargo test --doc --workspace`
  (`docs/features/kuberic/testing.md:206-208`).
- **Current Remove Unit Gate**:
  `cargo test -p kuberic-operator --features durable-remove-replica-pilot remove_replica_pilot`
  (`.github/workflows/CI.yml:66-71`).
- **Current Remove Reconciler Gate**:
  `cargo test -p kvstore --features durable-remove-replica-pilot --test reconciler test_durable_execution_remove_replica_pilot_`
  (`docs/features/kuberic/testing.md:508-516`).
- **Current Representative Measurement**:
  `cargo test -p kvstore --features durable-remove-replica-pilot --test reconciler test_durable_execution_remove_replica_pilot_three_no_fault_measurement_samples -- --nocapture`
  (`docs/features/kuberic/testing.md:518-524`).
- **Explicit Remove Regression**:
  `cargo test -p kvstore --test reconciler test_durable_remove_coarse_activation`
  (`docs/features/kuberic/testing.md:210-220`).
- **Kernel/Provider Tests**:
  `cargo test -p kuberic-durable-execution --features kubernetes --test kubernetes_checkpoint`;
  the real-provider command is
  `cargo test -p kuberic-durable-execution --features kubernetes --test kubernetes_checkpoint_real -- --nocapture`
  (`docs/features/kuberic/testing.md:480-497`).
- **Lint Command**: `cargo clippy --all-targets -- -D warnings`
  (`.github/workflows/CI.yml:54-55`).
- **Formatting Command**: `cargo fmt --all -- --check`
  (`.github/workflows/CI.yml:46-47`).
- **Build Command**: `cargo build --all-targets`
  (`.github/workflows/CI.yml:75-76`).
- **Type Check**: `cargo check --all-targets`
  (`.github/workflows/CI.yml:43-44`).
- **Full Workspace Gate**: `cargo test --all --all-features`
  (`.github/workflows/CI.yml:78-89`).
- **Kind/Image Surface**: create the `kind` cluster from
  `deploy/kind-config.yaml`, then run `just images`; the recipes build all
  workspace binaries and load `localhost/kuberic-operator:latest` and
  `localhost/kvstore:latest` (`justfile:1-39`,
  `.github/workflows/CI.yml:78-89`).

## Detailed Findings

### 1. Public Selection, Persisted Identity, and Routing

- `KubericSetSpec` contains `remove_replica_execution_mode`; its documentation
  states that omitted values remain explicit
  (`kuberic-operator/src/crd.rs:54-63`). `RemoveReplicaExecutionMode` defaults
  to `Explicit` and exposes `DurablePilot`
  (`kuberic-operator/src/crd.rs:218-227`).
- `KubericSetStatus` has both the generic explicit checkpoint
  `operation` and the remove-specific immutable pilot reference
  `durable_remove_replica_pilot`
  (`kuberic-operator/src/crd.rs:125-140`). The pilot reference stores its own
  version, execution ID, checkpoint name, and complete initial-operation JSON
  (`kuberic-operator/src/crd.rs:229-243`).
- `accept_remove_replica` validates the selected engine and writes exactly one
  of the two status forms. Explicit acceptance calls `start_remove_replica`;
  pilot acceptance calls `new_pilot_execution`
  (`kuberic-operator/src/reconciler.rs:265-356`).
- `Phase::RemovingReplica` routing recognizes the pilot only when
  `status.operation` is absent and the pilot reference is present; all other
  remove states enter the ordinary durable-operation reconciler
  (`kuberic-operator/src/reconciler.rs:1797-1840`).
- The operator feature is default-off and makes the kernel dependency and
  Kubernetes provider optional
  (`kuberic-operator/Cargo.toml:10-24`). The kvstore test crate forwards a
  separate remove-pilot feature (`examples/kvstore/Cargo.toml:6-10`).
- Main runtime construction is conditional on either pilot feature
  (`kuberic-operator/src/main.rs:32-48`). `ReconcilerState` shares one
  `DurableWorkflowRuntime` between switchover and remove when both are enabled
  (`kuberic-operator/src/reconciler.rs:80-144`).
- Generated deployment schema exposes both
  `spec.removeReplicaExecutionMode` and
  `status.durableRemoveReplicaPilot`
  (`kuberic-operator/deploy/deployment.yaml:70-77`,
  `kuberic-operator/deploy/deployment.yaml:132-156`).

### 2. Remove Domain State Machine and Safety Authority

- `start_remove_replica` validates a non-primary committed active secondary,
  exact target incarnation/UID, ScaleDown generation evidence, minimum
  membership, and retained quorum before constructing operation version 2
  (`kuberic-operator/src/durable/remove_replica.rs:42-148`,
  `kuberic-operator/src/crd.rs:323-391`).
- The frozen operation carries previous, target, and optional committed
  snapshots; operation mode; primary/target IDs; exact pod name and UID;
  target generation; pending action; frozen remove intent; commit evidence;
  cleanup proof; and typed unsafe disposition
  (`kuberic-operator/src/crd.rs:323-391`,
  `kuberic-operator/src/crd.rs:403-532`).
- `decide_remove_replica` sends any operation with commit evidence directly to
  the post-commit path. Pre-commit phases freeze one coarse intent, persist one
  pending `RemoveReplicaIntent`, await correlated evidence, or finalize a safe
  compensation (`kuberic-operator/src/durable/remove_replica.rs:150-231`).
- Intent freezing obtains exact primary authority, validates ScaleDown target
  authority, derives previous/reduced structural configurations, caps
  pre-commit attempts at three, and signs the complete intent
  (`kuberic-operator/src/durable/remove_replica.rs:233-373`).
- The core protocol independently validates protocol version, action and
  attempt identity, distinct primary/target identities, target UID equality,
  endpoints, structural configurations, limits, and exact signature
  (`kuberic-core/src/remove_replica.rs:13-129`). Its signature includes mode,
  epoch, generation, control version, identities, configurations, quorum,
  minimum, and deadlines (`kuberic-core/src/remove_replica.rs:184-231`).
- Remove progress makes commit, connection absence, retirement, compensation,
  and current-install dispatch explicit; validation rejects pre-commit
  post-commit evidence and retirement before exact connection absence
  (`kuberic-core/src/remove_replica.rs:251-363`). Terminal validation keeps
  committed-clean, committed-degraded, compensated, and incomplete
  compensation evidence distinct (`kuberic-core/src/remove_replica.rs:365-397`).
- No protocol change is required by the current architecture: the durable
  workflow prepares and dispatches the existing signed
  `RemoveReplicaIntent`, while exact pod label/delete proofs remain operator
  effects (`kuberic-operator/src/durable/effects.rs:45-104`,
  `kuberic-operator/src/durable/effects.rs:828-1014`).

### 3. Current Durable Remove Contract and Replay Path

- The current limits are 3 replicas, 16 history records, 48 transition fuel,
  8,192 workflow-input bytes, 16,384 activity-input bytes, 8,192
  activity-result bytes, 4,096 terminal-payload bytes, and a 752 KiB encoded
  checkpoint (`kuberic-operator/src/durable/remove_replica_pilot.rs:49-61`).
- Workflow input contains version, execution ID, and the complete initial
  `DurableOperationStatus`
  (`kuberic-operator/src/durable/remove_replica_pilot.rs:134-140`).
- Mutable state contains phase, committed snapshot, deadline, pending action,
  error, intent, commit evidence, cleanup, and disposition. `apply_to` clones
  the initial operation, overlays every mutable field, then revalidates the
  operation and immutable authority
  (`kuberic-operator/src/durable/remove_replica_pilot.rs:142-190`).
- Each activity input contains version, the entire mutable state, and either a
  passive observation or an exact replica/label/delete command
  (`kuberic-operator/src/durable/remove_replica_pilot.rs:192-226`).
- Each result contains the entire next mutable state. The
  proven-no-admission result adds action ID and redelivery count
  (`kuberic-operator/src/durable/remove_replica_pilot.rs:278-302`).
- The workflow replays deterministic transitions in memory, then issues one
  typed passive-observation call. The prepared resolver may replace that
  logical boundary with an exact external command
  (`kuberic-operator/src/durable/remove_replica_pilot.rs:335-478`,
  `kuberic-operator/src/durable/remove_replica_pilot.rs:920-1158`).
- Recorded prepared commands are accepted on replay only after semantic
  predecessor and exact-command validation
  (`kuberic-operator/src/durable/remove_replica_pilot.rs:1074-1158`).
  Prepared replica validation reconstructs the expected command and core
  intent signature; label and delete validation bind replica ID, pod name, and
  UID (`kuberic-operator/src/durable/remove_replica_pilot.rs:1166-1215`).
- Transition validation keeps immutable authority fixed, makes commit evidence
  monotonic, forbids cleanup before commit, preserves current-install dispatch
  evidence, constrains phase edges, and binds commit to the exact reduced
  snapshot (`kuberic-operator/src/durable/remove_replica_pilot.rs:1264-1376`).
- Terminal construction emits `Completed`, `Compensated`,
  `UnsafeAmbiguity`, or `Rejected`. Completed terminals include a commit
  authority, commit evidence, cleanup, and accounting
  (`kuberic-operator/src/durable/remove_replica_pilot.rs:305-329`,
  `kuberic-operator/src/durable/remove_replica_pilot.rs:555-624`).
- Loaded completed terminals rebind attempt/action identity, input signature,
  primary generation, reduced configuration, commit evidence, cleanup, and
  completed activity count before publication
  (`kuberic-operator/src/durable/remove_replica_pilot.rs:718-826`).

### 4. Kernel Persistence and Recovery Semantics

- The checkpoint envelope is format version 3, rejects unknown envelope
  fields, checks encoded size before decoding, and returns an explicit
  `UnsupportedFormat` for any other format
  (`durable-execution/src/checkpoint.rs:10-119`).
- The execution contract persists the exact `ExecutionSpec` and admitted
  encoded capacity. Validation compares execution identity, exact workflow
  input, terminal payload bound, and internal capacity/history constraints
  (`durable-execution/src/checkpoint.rs:127-145`,
  `durable-execution/src/checkpoint.rs:246-273`).
- Active checkpoints contain the full activity vector; terminal checkpoints
  contain only terminal outcome and completed activity count
  (`durable-execution/src/checkpoint.rs:154-214`). Terminalization is a
  validated active-to-terminal replacement
  (`durable-execution/src/checkpoint.rs:225-244`).
- Active history validation enforces record count, contiguous sequences, a
  single final pending record, per-record result bounds, total encoded bounds,
  admitted capacity, and terminal capacity
  (`durable-execution/src/checkpoint.rs:458-578`).
- `turn_and_expose_with` loads authoritative state, returns a stored terminal
  without polling, quarantines a final exposed record, resolves prepared
  activities, and persists a permit only after accepted CAS
  (`durable-execution/src/host.rs:326-445`,
  `durable-execution/src/host.rs:710-767`).
- `observe_and_turn_with` validates the exact exposed logical activity and
  result bound, then persists the observation together with the next exposed
  activity or terminal state in one CAS
  (`durable-execution/src/host.rs:540-686`).
- CAS conflict and outcome-unknown map to `ReloadRequired`; neither produces a
  later dispatch permit from stale local state
  (`durable-execution/src/host.rs:848-856`).

### 5. Operator Recovery Loop and Publication Ordering

- Durable remove reconciliation loads the checkpoint before collecting
  external observations and directly publishes an already validated terminal
  (`kuberic-operator/src/reconciler.rs:3481-3520`).
- Active operation reconstruction uses the final scheduled/exposed input or
  final completed result and overlays it on the immutable initial operation
  (`kuberic-operator/src/reconciler.rs:3939-3970`).
- The reconciler gathers current pod IDs/UIDs, role labels, replica handles,
  status, addresses, and addressed instances before creating the prepared
  resolver (`kuberic-operator/src/reconciler.rs:3521-3598`).
- The bounded loop handles passive evidence, prepared effects, one-use
  permits, fence-refresh results, quarantine, terminal validation,
  reload/store failures, preparation waits, and nondeterminism
  (`kuberic-operator/src/reconciler.rs:3597-3862`).
- Remove label/delete preparation and postconditions use exact UID identity.
  A replacement UID satisfies the old command's absence postcondition without
  mutating the replacement (`kuberic-operator/src/durable/effects.rs:899-990`).
- Terminal publication derives either the target or previous snapshot from
  the immutable initial operation, updates status, persists the status, records
  measurements, and then evicts only the process-local host
  (`kuberic-operator/src/reconciler.rs:3973-4127`).
- The integration fixture demonstrates that a failed status patch leaves a
  terminal checkpoint with the old three-member published topology; restart
  later publishes the two-member topology without polling or dispatching
  (`examples/kvstore/tests/reconciler.rs:7041-7130`).

### 6. Shared Runner Seams

#### Existing workflow-neutral seams

- `DurableActivity` owns activity name/version and independent input/result
  bounds; typed encoding canonicalizes JSON
  (`durable-execution/src/typed.rs:52-131`,
  `durable-execution/src/typed.rs:159-180`).
- `PreparedActivityResolver` receives both the logical request and any
  authoritative recorded exact request; the kernel performs the final
  byte-for-byte comparison (`durable-execution/src/typed.rs:27-50`).
- `HostOutcome` is one shared outcome vocabulary for schedule, permit,
  observation, completion, quarantine, nondeterminism, rejection, reload, and
  store failure (`durable-execution/src/host.rs:117-164`).
- `DispatchPermit` exposes only activity and attempt identity and cannot be
  constructed outside the host (`durable-execution/src/host.rs:89-112`).
- `DurablePermitGuard` adds operator-side one-use and exact-spec validation
  shared by current workflows
  (`kuberic-operator/src/durable/workflow_host.rs:23-62`).
- `DurableWorkflowRuntime` keys hosts by namespace, set name, set UID,
  workflow, and execution ID, while checkpoints remain authoritative
  (`kuberic-operator/src/durable/workflow_host.rs:73-155`).
- `DurableCheckpointStore` abstracts Kubernetes and in-memory providers;
  `CheckpointMeasurementDecoder` supplies workflow-specific activity and
  terminal accounting without changing the store
  (`kuberic-operator/src/durable/pilot_store.rs:14-93`).
- `ReplicaEffectCommand`, `LabelEffectCommand`, `DurableEffectOutcome`, and the
  bridge helpers form the shared operator effect vocabulary. The module
  explicitly remains in-process and owns no worker or scheduler
  (`kuberic-operator/src/durable/effects.rs:1-6`,
  `kuberic-operator/src/durable/effects.rs:45-173`).

#### Duplicated runner behavior

| Lifecycle concern | Switchover copy | Remove copy |
|---|---|---|
| Load, terminal short-circuit, active reconstruction | `kuberic-operator/src/reconciler.rs:2853-2871` | `kuberic-operator/src/reconciler.rs:3503-3520` |
| Observation/handle gathering and resolver creation | `kuberic-operator/src/reconciler.rs:2873-2933` | `kuberic-operator/src/reconciler.rs:3521-3598` |
| Fused turn and bounded outcome loop | `kuberic-operator/src/reconciler.rs:2931-3147` | `kuberic-operator/src/reconciler.rs:3596-3859` |
| Permit dispatch and passive observation | `kuberic-operator/src/reconciler.rs:2937-3049` | `kuberic-operator/src/reconciler.rs:3602-3743` |
| Quarantine resolution | `kuberic-operator/src/reconciler.rs:3051-3092` | `kuberic-operator/src/reconciler.rs:3746-3815` |
| Terminal validation/publication handoff | `kuberic-operator/src/reconciler.rs:3094-3110` | `kuberic-operator/src/reconciler.rs:3817-3835` |
| Reload/store/rejection routing | `kuberic-operator/src/reconciler.rs:3111-3147` | `kuberic-operator/src/reconciler.rs:3836-3859` |
| Deadline-clamped requeue | `kuberic-operator/src/reconciler.rs:3155-3166` | `kuberic-operator/src/reconciler.rs:3865-3872` |

#### Operation-specific responsibilities already visible at the seam

1. **Observation collection**: switchover requires all previous members to
   preserve incarnation and gathers target-snapshot members
   (`kuberic-operator/src/reconciler.rs:2873-2924`); remove gathers
   previous-snapshot members plus exact target UID/role evidence
   (`kuberic-operator/src/reconciler.rs:3521-3584`).
2. **Authority and prepared-effect resolution**:
   `PilotPreparedActivityResolver` and
   `RemoveReplicaPreparedActivityResolver` remain separate
   (`kuberic-operator/src/reconciler.rs:2926-2933`,
   `kuberic-operator/src/reconciler.rs:3586-3598`).
3. **Exact effect bridging and quarantine interpretation**: each workflow has
   distinct bridge and resolve functions, while both return the shared bridge
   outcomes (`kuberic-operator/src/reconciler.rs:2981-3060`,
   `kuberic-operator/src/reconciler.rs:3650-3761`).
4. **Deadline policy**: both currently choose pending-action deadline before
   operation deadline and clamp to one through ten seconds, but the helpers
   are workflow-local (`kuberic-operator/src/reconciler.rs:3155-3166`,
   `kuberic-operator/src/reconciler.rs:3865-3872`).
5. **Terminal validation**: switchover and remove decode different terminal
   contracts and enforce different authority proofs
   (`kuberic-operator/src/reconciler.rs:3094-3110`,
   `kuberic-operator/src/reconciler.rs:3817-3835`).
6. **Publication and conditions**: remove reconstructs membership from the
   initial remove snapshots and uses a remove-specific condition; switchover
   retains its own publication implementation
   (`kuberic-operator/src/reconciler.rs:3973-4127`,
   `kuberic-operator/src/reconciler.rs:4994-5040`).

The future add-replica operation already differs in mode (`ScaleUp` or
`Rebuild`), optional old-incarnation retirement, a 600-second deadline,
target build/copy/activate evidence, compensation, and degraded post-commit
publication (`kuberic-operator/src/durable/add_replica.rs:20-139`,
`kuberic-operator/src/durable/add_replica.rs:294-386`). The implemented
protocol documentation describes shared lifecycle identity/transport while
keeping add and remove intents, coordinators, configurations, commit rules,
and terminals independent
(`docs/features/kuberic/implemented/agent-owned-replica-add-build-protocol.md:53-63`,
`docs/features/kuberic/implemented/agent-owned-replica-remove-protocol.md:169-179`).

### 7. Compact-State Opportunities Present in the Current Shape

| Persisted material today | Existing location and repetition |
|---|---|
| Initial full operation | Stored as `initial_operation_json` in the status reference, decoded, then re-encoded inside workflow input (`kuberic-operator/src/durable/remove_replica_pilot.rs:1415-1499`). |
| Full mutable state in every boundary input | `DurableRemoveReplicaActivityInput` always contains `DurableRemoveReplicaState` (`kuberic-operator/src/durable/remove_replica_pilot.rs:142-203`). |
| Full next mutable state in every result | Both `Advance` and `ProvenNoAdmission` contain `DurableRemoveReplicaState` (`kuberic-operator/src/durable/remove_replica_pilot.rs:278-302`). |
| Pending action plus exact prepared replica command | The mutable state retains `PendingActionStatus`; `ReplicaEffectCommand` copies action ID, signature, target identity, epoch, generation, control version, runtime epoch, and payload from it (`kuberic-operator/src/durable/remove_replica_pilot.rs:148-151`, `kuberic-operator/src/durable/effects.rs:45-104`). |
| Three complete configuration descriptors | `RemoveReplicaIntentStatus` retains previous, reduced catch-up, and reduced current descriptors (`kuberic-operator/src/crd.rs:403-450`); each member descriptor includes IDs, roles, addresses, progress source, and progress fields (`kuberic-operator/src/crd.rs:536-556`). |
| Terminal commit authority plus commit evidence | `Completed` stores both structures (`kuberic-operator/src/durable/remove_replica_pilot.rs:305-313`); attempt/action/generation/configuration are repeated across them (`kuberic-operator/src/durable/remove_replica_pilot.rs:241-250`, `kuberic-operator/src/crd.rs:458-465`). |
| Retained complete active history | Active checkpoints retain the full vector of each input and completed result until terminal replacement (`durable-execution/src/checkpoint.rs:154-214`, `durable-execution/src/checkpoint.rs:529-564`). |

Existing compact boundary forms already identify the minimum externally
dispatchable material:

- `ReplicaEffectCommand` is the exact immutable replica mutation
  (`kuberic-operator/src/durable/effects.rs:45-104`).
- `LabelEffectCommand` is an exact replica ID/pod/UID/role mutation
  (`kuberic-operator/src/durable/effects.rs:107-146`).
- `DeleteEffectCommand` is an exact replica ID/pod/UID deletion
  (`kuberic-operator/src/durable/effects.rs:828-857`).
- Commit evidence and cleanup are already separate typed structures
  (`kuberic-operator/src/crd.rs:458-480`).
- Terminal checkpoints already discard active history and retain only outcome
  plus activity count (`durable-execution/src/checkpoint.rs:154-214`,
  `kuberic-operator/src/durable/remove_replica_pilot.rs:4349-4387`).

The current admission model can enforce smaller contracts without changing
the provider abstraction: activity types own input/result bounds, execution
spec owns terminal-payload capacity, and checkpoint limits own history count
and total encoded size (`durable-execution/src/typed.rs:125-131`,
`durable-execution/src/checkpoint.rs:13-37`,
`durable-execution/src/checkpoint.rs:127-145`). The current remove-specific
maximum-payload fixture constructs every activity at its full declared input
and result size and every history slot at the maximum count
(`kuberic-operator/src/durable/remove_replica_pilot.rs:1668-1719`).

### 8. Compatibility and Fail-Closed Handling

- Compatibility is layered:
  - kernel envelope format version 3
    (`durable-execution/src/checkpoint.rs:10-10`);
  - remove pilot reference/workflow version 1
    (`kuberic-operator/src/durable/remove_replica_pilot.rs:49-49`);
  - explicit remove operation version 2
    (`kuberic-operator/src/crd.rs:323-326`).
- `execution_id` rejects a pilot reference with another version and rejects a
  checkpoint name not deterministically derived from that execution
  (`kuberic-operator/src/durable/remove_replica_pilot.rs:1455-1470`).
- Workflow and activity decoding independently reject unsupported remove
  versions (`kuberic-operator/src/durable/remove_replica_pilot.rs:342-364`,
  `kuberic-operator/src/durable/remove_replica_pilot.rs:659-670`).
- Checkpoint loading rejects unsupported format, malformed JSON, another
  execution ID, changed exact workflow input, changed terminal bound, invalid
  history, and capacity mismatch before polling the workflow
  (`durable-execution/src/checkpoint.rs:103-119`,
  `durable-execution/src/checkpoint.rs:246-273`,
  `durable-execution/tests/replay.rs:443-515`).
- Serde contracts use `deny_unknown_fields` for the reference, workflow input,
  mutable state, activity input/result, terminal, exact commands, envelope,
  execution contract, and checkpoint payload
  (`kuberic-operator/src/crd.rs:229-243`,
  `kuberic-operator/src/durable/remove_replica_pilot.rs:134-203`,
  `kuberic-operator/src/durable/remove_replica_pilot.rs:278-329`,
  `durable-execution/src/checkpoint.rs:42-45`,
  `durable-execution/src/checkpoint.rs:127-159`).
- The current pilot terminal vocabulary has no `Incompatible` variant; decode
  and version failures currently become errors or `Rejected`
  (`kuberic-operator/src/durable/remove_replica_pilot.rs:305-329`,
  `kuberic-operator/src/durable/remove_replica_pilot.rs:342-364`,
  `kuberic-operator/src/reconciler.rs:3849-3859`). This is the existing
  contract distinction relevant to the specification's required typed
  incompatible outcome.
- The explicit operation already has a clean-break test: version 1 is rejected
  rather than migrated (`kuberic-operator/src/durable/remove_replica.rs:1939-1962`).

### 9. Checkpoint Ownership, Retention, and Measurements

- Remove creates a same-namespace non-controlling, non-blocking owner
  reference using the exact `KubericSet` name and UID
  (`kuberic-operator/src/durable/remove_replica_pilot.rs:1509-1532`).
- The Kubernetes provider validates non-empty owner fields, rejects controlling
  or blocking ownership, and rejects a namespaced owner outside the checkpoint
  namespace (`durable-execution/src/kubernetes.rs:424-476`).
- Loads and replacements require the configured owner relationship to match
  the persisted relationship exactly
  (`durable-execution/src/kubernetes.rs:393-416`,
  `durable-execution/tests/kubernetes_checkpoint.rs:303-499`).
- The writer identity has only ConfigMap `get`, `create`, and `update`; the
  separately named cleanup identity has only `list` and `delete`
  (`durable-execution/deploy/checkpoint-writer-rbac.yaml:1-25`,
  `durable-execution/deploy/checkpoint-cleanup-rbac.yaml:1-33`).
- A real-API test creates an owner-bound checkpoint, deletes the owner, and
  waits for checkpoint absence
  (`durable-execution/tests/kubernetes_checkpoint_real.rs:285-336`).
- Measurements classify each completed boundary as external or passive and
  validate terminal accounting against completed activity count
  (`kuberic-operator/src/durable/remove_replica_pilot.rs:680-711`).
  The store tracks accepted writes and active/terminal byte ranges and
  correlates them with host persistence boundaries
  (`kuberic-operator/src/durable/pilot_store.rs:95-120`,
  `kuberic-operator/src/durable/pilot_store.rs:280-324`).
- The representative integration fixture asserts 3 external effects, 2
  passive observations, 5 durable boundaries, and 6 accepted writes, while
  printing active checkpoint, terminal checkpoint, and terminal payload sizes
  separately (`examples/kvstore/tests/reconciler.rs:7200-7297`).

## Safety-Test Traceability

The table maps every inventory item in specification SC-002 to current
assertions. “Native present” means the current pilot already has direct
coverage. “Shared” means the assertion is in the durable kernel/provider.
“Explicit baseline” identifies behavior that is presently asserted only or
more completely in the explicit reconciler suite.

| SC-002 inventory item | Current assertion(s) | Current trace state |
|---|---|---|
| Immutable operation mode | `remove_replica_pilot_preserves_scale_down_authority_and_never_converts_to_force` and Force fence test (`kuberic-operator/src/durable/remove_replica_pilot.rs:2509-2546`); explicit target-loss matrix (`examples/kvstore/tests/reconciler.rs:7808-7872`). | Native present. |
| Pre-commit restart | Every-boundary unit transcript (`kuberic-operator/src/durable/remove_replica_pilot.rs:3359-3840`) and every-turn reconciler restart (`examples/kvstore/tests/reconciler.rs:6717-6758`). | Native present. |
| Post-commit restart | `remove_replica_pilot_restart_after_commit_rolls_forward` (`kuberic-operator/src/durable/remove_replica_pilot.rs:4231-4245`); explicit primary-restart roll-forward (`examples/kvstore/tests/reconciler.rs:8491-8588`). | Native unit plus explicit end-to-end. |
| Exact prepared command | Exact coarse identity and exposure tests (`kuberic-operator/src/durable/remove_replica_pilot.rs:2548-2585`, `kuberic-operator/src/durable/remove_replica_pilot.rs:3968-3990`). | Native present. |
| Direct-dispatch authority | Lifecycle payload cannot pass the direct-command decoder (`kuberic-operator/src/durable/remove_replica_pilot.rs:2587-2612`). | Native present. |
| One-use dispatch authority | Replica and delete permits reject second consumption (`kuberic-operator/src/durable/remove_replica_pilot.rs:4027-4088`). | Native present. |
| Lost effect reply | Pilot lost-reply reconciler fixture (`examples/kvstore/tests/reconciler.rs:6763-6777`) and exact exposed-command quarantine (`kuberic-operator/src/durable/remove_replica_pilot.rs:4144-4175`). | Native present. |
| Uncertain persistence write | Outcome-unknown-after-apply reload/quarantine (`kuberic-operator/src/durable/remove_replica_pilot.rs:4389-4435`; `examples/kvstore/tests/reconciler.rs:6853-6887`). | Native present. |
| Conflict reload | Conflict-without-apply returns reload and later completes (`kuberic-operator/src/durable/remove_replica_pilot.rs:4389-4435`; `examples/kvstore/tests/reconciler.rs:6815-6848`). | Native present. |
| Authoritative observation recovery | Restart after a persisted observation replays to the exact next command (`kuberic-operator/src/durable/remove_replica_pilot.rs:4177-4229`); terminal reload skips workflow polling (`kuberic-operator/src/durable/remove_replica_pilot.rs:4286-4327`). | Native present. |
| Exact primary-status gap without churn | Explicit post-commit fixture preserves checkpoint/status count and connection proof (`examples/kvstore/tests/reconciler.rs:8590-8702`). Pilot evidence collection waits when authority is missing (`kuberic-operator/src/durable/remove_replica_pilot.rs:2614-2661`). | Explicit end-to-end baseline; native no-churn fixture not separately named. |
| Exact target-status gap without churn | Explicit preadmission fixture preserves Healthy state/status count on repeated target unavailability (`examples/kvstore/tests/reconciler.rs:7635-7740`). Pilot ScaleDown test retains mode and rejects missing target (`kuberic-operator/src/durable/remove_replica_pilot.rs:2509-2524`). | Explicit end-to-end baseline; native no-churn fixture not separately named. |
| Commit evidence | Commit-before-cleanup test and frozen-authority mutation rejection (`kuberic-operator/src/durable/remove_replica_pilot.rs:2769-2812`). | Native present. |
| Configuration authority | Generation/control-version/runtime-epoch/protocol/configuration drift matrix (`kuberic-operator/src/durable/remove_replica_pilot.rs:2459-2507`). | Native present. |
| Correlated primary role evidence | Correlated evidence test requires primary/role data (`kuberic-operator/src/durable/remove_replica_pilot.rs:2614-2661`). | Native present. |
| Correlated lifecycle evidence | Same correlated evidence test requires lifecycle and retirement proof (`kuberic-operator/src/durable/remove_replica_pilot.rs:2614-2661`); terminal cleanup ordering test (`kuberic-operator/src/durable/remove_replica_pilot.rs:2814-2843`). | Native present. |
| UID-fenced label cleanup | Exact label postcondition test and replacement-UID handling (`kuberic-operator/src/durable/remove_replica_pilot.rs:4437-4496`); explicit replacement-pod fixture (`examples/kvstore/tests/reconciler.rs:7538-7633`). | Native unit plus explicit end-to-end. |
| UID-fenced deletion | Exact delete postcondition and cleanup restart tests (`kuberic-operator/src/durable/remove_replica_pilot.rs:4247-4284`, `kuberic-operator/src/durable/remove_replica_pilot.rs:4437-4496`). | Native present. |
| Post-commit connection cleanup | Completion requires connection absence and retirement before publication (`kuberic-operator/src/durable/remove_replica_pilot.rs:2814-2843`); explicit missing-primary-status fixture never waives it (`examples/kvstore/tests/reconciler.rs:8590-8702`). | Native unit plus explicit end-to-end. |
| Incarnation fencing | Preparation rejects primary/target incarnation and UID drift (`kuberic-operator/src/durable/remove_replica_pilot.rs:4090-4142`); reconciler stale-incarnation test (`examples/kvstore/tests/reconciler.rs:6921-6946`). | Native present. |
| Epoch fencing | Authority drift matrix mutates runtime epoch (`kuberic-operator/src/durable/remove_replica_pilot.rs:2459-2507`). | Native present. |
| Bounded redrive at most three attempts | Safe-configuration redrive stops at `MAX_REMOVE_REPLICA_PRE_COMMIT_ATTEMPTS` (`kuberic-operator/src/durable/remove_replica_pilot.rs:2704-2767`). | Native present. |
| Corrupt record handling | Kernel rejects invalid sequence/pending shape and malformed declarations before polling (`durable-execution/tests/replay.rs:381-425`, `durable-execution/tests/support/scenarios.rs:3180-3315`); Kubernetes provider rejects malformed stored JSON (`durable-execution/tests/kubernetes_checkpoint.rs:573-627`). | Shared kernel/provider; no remove-specific fixture named. |
| Incompatible record handling | Kernel rejects unsupported envelope/input identity before polling (`durable-execution/tests/replay.rs:443-515`); explicit remove rejects v1 (`kuberic-operator/src/durable/remove_replica.rs:1939-1962`). | Shared plus explicit baseline; current remove terminal has no typed incompatible variant. |
| Distinct unsafe terminal handling | Success, compensation, and unsafe ambiguity map to different terminal variants (`kuberic-operator/src/durable/remove_replica_pilot.rs:2845-2871`, `kuberic-operator/src/durable/remove_replica_pilot.rs:4329-4347`). | Native present. |
| Distinct inexact terminal handling | Completed terminal rejects changed action, signature, configuration, and nonterminal retirement (`kuberic-operator/src/durable/remove_replica_pilot.rs:2873-2938`). | Native present. |
| Terminal-before-status ordering | Status-failure and ordering fixtures prove terminal storage precedes publication and reload dispatches nothing (`examples/kvstore/tests/reconciler.rs:7041-7130`). | Native present. |
| Redelivery without duplicate uncertain effects | Exposed restart yields quarantine without a second permit (`kuberic-operator/src/durable/remove_replica_pilot.rs:4144-4175`); proven-no-admission permits exactly one fresh dispatch (`examples/kvstore/tests/reconciler.rs:6782-6810`). | Native present. |
| Three-member admission | Constant and admission validator cap members at three (`kuberic-operator/src/durable/remove_replica_pilot.rs:49-50`, `kuberic-operator/src/durable/remove_replica_pilot.rs:1534-1585`); admission drift test covers primary/nonmember/UID/minimum/quorum (`kuberic-operator/src/durable/remove_replica_pilot.rs:2394-2457`). | Native present. |
| Active admission | Remove projected-history fixture and kernel loaded-history/encoded-limit tests (`kuberic-operator/src/durable/remove_replica_pilot.rs:4498-4522`, `durable-execution/tests/replay.rs:517-575`). | Native plus shared. |
| Terminal admission | Terminal variant sizing and maximum terminal fixture (`kuberic-operator/src/durable/remove_replica_pilot.rs:1587-1669`, `kuberic-operator/src/durable/remove_replica_pilot.rs:4498-4522`); kernel terminal projection (`durable-execution/src/checkpoint.rs:274-306`). | Native plus shared. |
| Persisted-record owner identity | Remove binds owner UID in store options (`kuberic-operator/src/durable/remove_replica_pilot.rs:1509-1532`); provider preserves and rejects changed/missing/additional owners (`durable-execution/tests/kubernetes_checkpoint.rs:303-499`). | Shared provider with remove construction path. |
| Owner garbage collection | Real-API owner deletion removes the checkpoint (`durable-execution/tests/kubernetes_checkpoint_real.rs:285-336`). | Shared real-provider fixture. |
| Retained-record cleanup authorization | Writer RBAC omits delete and cleanup RBAC separates `list`/`delete` (`durable-execution/deploy/checkpoint-writer-rbac.yaml:1-25`, `durable-execution/deploy/checkpoint-cleanup-rbac.yaml:1-33`); provider documentation assigns orphan policy to the separate actor (`durable-execution/README.md:198-220`). | Static manifest/documentation coverage; no executable authorization-policy fixture. |

## Affected Tests, Build Surfaces, and Documentation

### Production/build surfaces

- `kuberic-operator/Cargo.toml:10-24` — optional remove pilot feature and
  kernel dependency activation.
- `examples/kvstore/Cargo.toml:6-10` — forwarded test feature.
- `kuberic-operator/src/durable/mod.rs:12-30` — feature-gated remove module,
  store, and host exports.
- `kuberic-operator/src/main.rs:32-48` — conditional runtime construction.
- `kuberic-operator/src/crd.rs:54-63`, `kuberic-operator/src/crd.rs:125-140`,
  `kuberic-operator/src/crd.rs:218-243` — selector, pilot status field, and
  reference type.
- `kuberic-operator/src/reconciler.rs:80-180`,
  `kuberic-operator/src/reconciler.rs:265-356`,
  `kuberic-operator/src/reconciler.rs:1797-1840`,
  `kuberic-operator/src/reconciler.rs:3481-4127`,
  `kuberic-operator/src/reconciler.rs:4994-5090` — runtime fields, acceptance,
  routing, runner copy, publication, conditions, and host cleanup.
- `kuberic-operator/src/durable/remove_replica_pilot.rs:1-4524` — current
  workflow, contract, admission, replay, terminal, and unit tests.
- `kuberic-operator/src/durable/remove_replica.rs:1-3060` — shared domain
  decision implementation plus explicit tests; the pure decision functions
  are also used by the pilot
  (`kuberic-operator/src/durable/remove_replica_pilot.rs:43-47`).
- `kuberic-operator/src/durable/effects.rs:1-1102` — shared and
  remove-specific exact effect adapters.
- `kuberic-operator/deploy/deployment.yaml:70-77`,
  `kuberic-operator/deploy/deployment.yaml:132-156` — generated CRD schema.
- `.github/workflows/CI.yml:58-89` — switchover measurement, remove pilot,
  build, Kind/image, and all-feature gates.
### Test surfaces

- Native remove unit and replay matrix:
  `kuberic-operator/src/durable/remove_replica_pilot.rs:1771-4524`.
- Explicit remove unit baseline:
  `kuberic-operator/src/durable/remove_replica.rs:1667-3047`.
- Native remove reconciler and measurement matrix:
  `examples/kvstore/tests/reconciler.rs:6543-7297`.
- Explicit remove reconciler/lifecycle matrix:
  `examples/kvstore/tests/reconciler.rs:7301-9074`.
- Shared switchover runner regression matrix:
  `examples/kvstore/tests/reconciler.rs:3968-5672` and
  `kuberic-operator/src/durable/pilot.rs:2230-3898`.
- Add-replica regressions exercising the ordinary durable reconciler:
  `examples/kvstore/tests/reconciler.rs:5889-6454`.
- Failover regressions:
  `examples/kvstore/tests/reconciler.rs:9594-10172`.
- Kernel replay, bounds, malformed contract, provider, owner, and real-API
  tests: `durable-execution/tests/replay.rs:381-610`,
  `durable-execution/tests/support/scenarios.rs:3180-3315`,
  `durable-execution/tests/kubernetes_checkpoint.rs:180-950`,
  `durable-execution/tests/kubernetes_checkpoint_real.rs:256-336`.

### Documentation surfaces

- `durable-execution/README.md:180-220`,
  `durable-execution/README.md:403-421` — retention model and closed pilot
  evaluation.
- `docs/features/kuberic/durable-execution-roadmap.md:34-50`,
  `docs/features/kuberic/durable-execution-roadmap.md:216-269` — retained
  explicit defaults, pilot-era evaluation, and current byte measurements.
- `docs/features/kuberic/testing.md:499-547` — pilot naming, commands,
  boundaries/writes, and active/terminal measurements.
- `docs/features/kuberic/protocols.md:150-170` — build feature plus
  per-resource durable-pilot flow.
- `docs/features/kuberic/operator.md:263-297` — pilot execution model,
  no-worker boundary, owner, and publication behavior.
- `docs/features/kuberic/implemented/agent-owned-replica-remove-protocol.md:40-85`,
  `docs/features/kuberic/implemented/agent-owned-replica-remove-protocol.md:169-179`
  — remove versions, compatibility stance, and shared lifecycle-peer scope.
- `kuberic-operator/README.md:22-58` documents the switchover pilot only; it
  does not currently document the remove pilot.

## Architecture Documentation

The current architecture is an in-process controller adapter over a replay
kernel:

```text
KubericSet status reference
  -> workflow-keyed DurableWorkflowRuntime host
  -> ConfigMap/InMemory CheckpointStore
  -> format-3 active checkpoint
  -> prepared exact activity
  -> accepted CAS creates private DispatchPermit
  -> operator bridge invokes ReplicaHandle or exact-UID Kubernetes effect
  -> authoritative observation
  -> fused observation + next exposure or terminal CAS
  -> terminal reload and operation-specific status/topology publication
```

The runtime and kernel do not introduce another executor. Reconciliation is
the scheduler; `ReplicaAgent` remains the admission, fencing, action-ledger,
and duplicate-replay owner
(`kuberic-operator/src/durable/effects.rs:1-6`,
`docs/features/kuberic/operator.md:289-297`).

The stable implementation boundary for planning is therefore:

- common host lifecycle and outcome routing;
- workflow-specific evidence collection;
- workflow-specific logical-to-exact activity preparation;
- workflow-specific permit dispatch and quarantine resolution;
- workflow-specific deadline;
- workflow-specific terminal validation;
- workflow-specific publication and conditions.

Those responsibilities are directly visible in the paired reconciler loops
and existing kernel/operator traits
(`kuberic-operator/src/reconciler.rs:2834-3203`,
`kuberic-operator/src/reconciler.rs:3481-3912`,
`durable-execution/src/typed.rs:27-50`,
`durable-execution/src/host.rs:117-164`).

## Planning Implications

- One persisted remove identity must replace the current selector plus two
  status shapes; current selection and routing are concentrated in CRD,
  acceptance, phase routing, runtime wiring, and generated schema.
- The shared runner boundary is the duplicated host-outcome loop, not the
  remove decision engine or replica protocol. Observation, authority,
  exact-effect, deadline, terminal, and publication behavior remain
  operation-specific.
- Compactness work is centered on the repeated mutable state in activity
  inputs/results, duplicated initial-operation serialization, repeated
  pending/prepared command fields, full configuration descriptors, and
  duplicated terminal authority/evidence.
- Compatibility must distinguish kernel envelope format, workflow contract,
  and remove domain-operation version. The current terminal model has no typed
  incompatibility variant even though the kernel and explicit operation
  already reject incompatible inputs before execution.
- Explicit-code deletion is gated by the SC-002 trace matrix. The current
  native suite already covers most items; exact primary/target status-gap
  no-churn behavior, remove-specific corrupt/incompatible records, and
  executable retained-cleanup authorization are currently represented by
  explicit, shared-kernel/provider, or static-manifest coverage rather than
  separately named native remove fixtures.
- The representative measurement fixture is the canonical semantic-count and
  byte-reporting surface; current CI and documentation distinguish external
  effects, passive observations, durable boundaries, accepted writes, active
  record size, terminal record size, and terminal payload size.
- Shared-runner validation necessarily includes switchover; ordinary
  add-replica and failover regressions remain affected integration surfaces,
  while add-replica itself remains on its existing durable operation path.

## Open Questions

None. The repository contains implementation and test evidence for the
requested research areas. This commit is not present on a remote-tracking
branch, so file:line citations are used without GitHub permalinks.
