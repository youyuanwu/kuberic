---
date: 2026-09-07 20:19:33 UTC
git_commit: 1c7287038f5ca420f4364f56ab869faecdabad85
main_commit: 9f034a178d761468f727f934de5b6c2c26c73dd8
branch: feature/framework-native-remove-replica
repository: github.com/youyuanwu/kuberic
topic: "Framework-Native Remove Replica Spec Research"
tags: [research, specification]
status: complete
historical_snapshot: true
---

# Spec Research: Framework-Native Remove Replica

> **Historical PAW research snapshot:** Findings describe the pre-migration
> tree at the commits recorded above. They are retained as specification
> evidence, not as current product documentation.

## Summary

The merged `main` implementation has two remove-replica operator paths. The
explicit CRD-backed state machine is the default; a build feature plus
`spec.removeReplicaExecutionMode: durablePilot` selects a kernel-hosted path.
Both paths use the same remove-replica decision logic and the same
`ReplicaAgent`/lifecycle protocol, but the durable path adds a separate CRD
reference, checkpoint host, reconciler loop, terminal publication path,
conditions, measurements, deployment schema, feature gates, tests, and pilot
documentation.

The durable kernel already supplies typed bounded calls, prepared activity
resolution, fused checkpoint progression, CAS reload semantics, quarantine,
terminal compaction, and a workflow-keyed host cache. The remaining runner
logic is substantially duplicated between switchover and remove-replica in the
operator reconciler. The current remove contract repeats its mutable state in
activity inputs and results and repeats commit-authority fields in terminal
records. A fresh run of the merged PR #59 fixture confirmed 6 accepted writes,
an aggregate active-checkpoint range of 5,013-93,841 bytes, terminal checkpoints
of 8,121-8,129 bytes, and a 2,188-byte terminal payload.

## Agent Notes

This work is a committed production migration, not another explicit-versus-durable comparison. Remove-replica must become the default and only operator orchestration path. The implementation must preserve the existing safety inventory, replace pilot-specific dual-path surfaces, materially reduce avoidable checkpoint payload repetition, and establish a reusable production runner suitable for a later add-replica migration. Breaking API changes are allowed, but old checkpoints must never be silently misinterpreted. `ReplicaAgent` and gRPC behavior remain out of scope unless research reveals a protocol defect that requires a material user decision.

## Research Findings

### Question 1: Where are the explicit and durable paths selected, hosted, recovered, and reconciled, and which surfaces exist only for dual-path pilot operation?

**Answer**:

- Selection has two gates. The operator crate exposes a default-off
  `durable-remove-replica-pilot` feature
  (`kuberic-operator/Cargo.toml:10-19`), while the CRD field defaults to
  `Explicit` and permits `DurablePilot`
  (`kuberic-operator/src/crd.rs:54-63`,
  `kuberic-operator/src/crd.rs:218-235`). Acceptance branches on that field:
  explicit acceptance writes `status.operation`; pilot acceptance writes
  `status.durableRemoveReplicaPilot` and clears `status.operation`
  (`kuberic-operator/src/reconciler.rs:265-350`).
- The explicit path is hosted by the ordinary durable-operation reconciler.
  The pilot is hosted by a feature-gated `DurableRemoveReplicaPilotRuntime`
  over the shared `DurableWorkflowRuntime`
  (`kuberic-operator/src/durable/remove_replica_pilot.rs:65-132`,
  `kuberic-operator/src/durable/workflow_host.rs:21-155`). Production wiring
  constructs one shared durable runtime only when either pilot feature is built
  (`kuberic-operator/src/main.rs:30-45`,
  `kuberic-operator/src/reconciler.rs:80-144`).
- Recovery routing is persisted-status driven. `RemovingReplica` with an
  ordinary operation follows the explicit reconciler; `RemovingReplica` with
  no operation and a pilot reference enters the pilot reconciler
  (`kuberic-operator/src/reconciler.rs:1797-1830`). The pilot first loads the
  checkpoint, publishes a validated terminal directly when present, or
  reconstructs the current operation from the final scheduled, exposed, or
  completed activity record (`kuberic-operator/src/reconciler.rs:3481-3520`,
  `kuberic-operator/src/reconciler.rs:3917-3970`).
- Pilot-only dual-path surfaces are:
  `RemoveReplicaExecutionMode`, `DurableRemoveReplicaPilotStatus`, and
  `status.durableRemoveReplicaPilot`
  (`kuberic-operator/src/crd.rs:132-139`,
  `kuberic-operator/src/crd.rs:218-235`); the feature-gated pilot module and
  runtime fields (`kuberic-operator/src/durable/mod.rs:12-29`,
  `kuberic-operator/src/reconciler.rs:87-90`); the
  `DurableRemoveReplicaPilot` condition and process-summary telemetry
  (`kuberic-operator/src/reconciler.rs:4059-4098`,
  `kuberic-operator/src/reconciler.rs:4993-5040`); generated CRD schema fields
  (`kuberic-operator/deploy/deployment.yaml:70-76`,
  `kuberic-operator/deploy/deployment.yaml:132-156`); the example crate feature
  (`examples/kvstore/Cargo.toml:7-10`); and separate CI gates
  (`.github/workflows/CI.yml:66-73`).

**Evidence**: Current CRD, operator feature configuration, reconciler routing,
runtime wiring, generated deployment schema, and CI at merged `main`
`9f034a178d761468f727f934de5b6c2c26c73dd8`.

**Implications**: Making framework-native removal the only path removes the
runtime selection decision and all remove-specific pilot naming/status/build
surfaces, while recovery still needs an unambiguous persisted execution
identity and checkpoint reference.

### Question 2: What durable runner machinery is duplicated between switchover and remove-replica?

**Answer**:

- Shared today: the workflow-keyed host cache and provider construction
  (`kuberic-operator/src/durable/workflow_host.rs:21-155`), measured checkpoint
  store and workflow-provided measurement decoder
  (`kuberic-operator/src/durable/pilot_store.rs:14-93`), typed activities and
  prepared resolution (`durable-execution/src/typed.rs:7-65`,
  `durable-execution/src/typed.rs:114-170`), fused schedule/exposure and
  observation/progression (`durable-execution/src/host.rs:333-410`,
  `durable-execution/src/host.rs:540-674`), and CAS conflict/unknown-outcome
  conversion to reload (`durable-execution/src/host.rs:848-856`).
- Duplicated in the reconciler: load and terminal short-circuit; reconstruct
  current operation; collect pods, handles, observations, and addressed
  instances; create a workflow-specific resolver; call
  `turn_and_expose_with`; loop over bounded activity fuel; decode and validate
  permits; fuse passive observations; bridge external effects; handle
  proven-no-admission refresh; resolve quarantine; validate and publish
  terminal outcomes; map reload/store failures to conditions; reject unfused
  outcomes; and compute deadline-clamped requeues. The switchover copy occupies
  `kuberic-operator/src/reconciler.rs:2834-3203`; the remove copy occupies
  `kuberic-operator/src/reconciler.rs:3479-3912`.
- Both loops match the same `HostOutcome` vocabulary—permit, quarantine,
  completion, reload, store failure, nondeterminism, checkpoint rejection, and
  observation rejection (`durable-execution/src/host.rs:130-164`)—and both
  clamp normal deadline fallback to 1-10 seconds
  (`kuberic-operator/src/reconciler.rs:3155-3166`,
  `kuberic-operator/src/reconciler.rs:3865-3876`). Both use workflow-local
  wrappers for loaded-terminal interpretation, active-state interpretation,
  status conditions, and terminal handoff.

**Evidence**: Side-by-side current switchover and remove pilot reconciler
blocks plus the shared durable kernel/host interfaces.

**Implications**: The reusable production runner scope is the common
host-outcome/reload/quarantine/permit/terminal/requeue control loop. Workflow
decision, observation gathering, effect preparation/dispatch, terminal
validation, and final status publication remain operation-specific behaviors.

### Question 3: What is the current durable remove workflow contract, what repeats, and what safety proofs consume it?

**Answer**:

- Workflow input contains version, execution ID, and the full initial
  `DurableOperationStatus`. Mutable workflow state contains phase, committed
  snapshot, deadline, pending action, error, frozen intent, commit evidence,
  cleanup, and disposition
  (`kuberic-operator/src/durable/remove_replica_pilot.rs:134-190`).
- Every activity input contains the full mutable state plus one activity kind.
  Every successful activity result contains the full next mutable state;
  proven-no-admission also returns that state plus action identity and
  redelivery count
  (`kuberic-operator/src/durable/remove_replica_pilot.rs:191-227`,
  `kuberic-operator/src/durable/remove_replica_pilot.rs:278-302`). Prepared
  replica activities additionally retain an exact command whose payload is
  copied from the pending action
  (`kuberic-operator/src/durable/effects.rs:45-104`).
- The full initial operation is also serialized into the CRD pilot reference,
  then decoded and serialized again into the immutable execution input
  (`kuberic-operator/src/durable/remove_replica_pilot.rs:1415-1499`).
- Completed terminals repeat attempt/action identity, input signature, primary
  generation, and configuration signature in `authority`, then repeat several
  of those fields in `commit_evidence`; they also carry cleanup and activity
  accounting (`kuberic-operator/src/durable/remove_replica_pilot.rs:241-329`).
- Safety consumers are explicit: applying mutable state validates the complete
  operation and immutable authority
  (`kuberic-operator/src/durable/remove_replica_pilot.rs:159-189`);
  transition validation prevents invented/regressed commit and cleanup
  evidence (`kuberic-operator/src/durable/remove_replica_pilot.rs:1264-1376`);
  prepared-command validation preserves coarse action and exact UID identity
  (`kuberic-operator/src/durable/remove_replica_pilot.rs:1166-1215`); loaded
  terminal validation binds action identity, primary generation, reduced
  configuration, commit evidence, cleanup, and activity count
  (`kuberic-operator/src/durable/remove_replica_pilot.rs:739-826`).

**Evidence**: Current serde contracts and their admission, transition,
prepared-command, and terminal validators.

**Implications**: Contract compaction must retain the information used by
immutable-authority, irreversible-commit, exact-command, cleanup, and terminal
accounting proofs. The avoidable repetition is the repeated mutable state in
both sides of every activity and the duplicated terminal commit-authority
fields.

### Question 4: What explicit safety and regression tests exist, and where is each safety invariant asserted?

**Answer**:

The current pilot-local safety inventory is directly represented by these
tests:

| Safety invariant | Current assertion |
|---|---|
| Admission, non-primary target, UID/incarnation, minimum and quorum | `remove_replica_pilot_rejects_primary_nonmember_uid_incarnation_minimum_and_quorum_drift` (`kuberic-operator/src/durable/remove_replica_pilot.rs:2394`) |
| Primary generation/control-version/runtime-epoch/protocol/configuration authority | `remove_replica_pilot_rejects_generation_control_version_runtime_epoch_protocol_and_configuration_drift` (`kuberic-operator/src/durable/remove_replica_pilot.rs:2459`) |
| ScaleDown never becomes Force; Force keeps topology/primary fences | `kuberic-operator/src/durable/remove_replica_pilot.rs:2509`, `kuberic-operator/src/durable/remove_replica_pilot.rs:2526` |
| Exact coarse command identity and direct-command boundary | `kuberic-operator/src/durable/remove_replica_pilot.rs:2548`, `kuberic-operator/src/durable/remove_replica_pilot.rs:2587` |
| Correlated primary, connection, lifecycle, UID, and role evidence | `kuberic-operator/src/durable/remove_replica_pilot.rs:2614` |
| Refresh/busy/conflict/unknown/proven-no-admission distinction | `kuberic-operator/src/durable/remove_replica_pilot.rs:2663` |
| Redrive only from safe configuration and within three attempts | `kuberic-operator/src/durable/remove_replica_pilot.rs:2704` |
| Commit persisted before cleanup; no rollback after dispatch ambiguity | `kuberic-operator/src/durable/remove_replica_pilot.rs:2769` |
| Exact connection, retirement, UID label and UID delete before publication | `kuberic-operator/src/durable/remove_replica_pilot.rs:2814` |
| Completed, compensated, unsafe, and inexact terminal handling | `kuberic-operator/src/durable/remove_replica_pilot.rs:2845`, `kuberic-operator/src/durable/remove_replica_pilot.rs:2873` |
| Deterministic success/compensation and restart at every boundary | `kuberic-operator/src/durable/remove_replica_pilot.rs:2940`, `kuberic-operator/src/durable/remove_replica_pilot.rs:3208`, `kuberic-operator/src/durable/remove_replica_pilot.rs:3359` |
| Typed bounds, deterministic mismatch, exposure-before-dispatch, one-use permit, drift rejection | `kuberic-operator/src/durable/remove_replica_pilot.rs:3852`, `kuberic-operator/src/durable/remove_replica_pilot.rs:3877`, `kuberic-operator/src/durable/remove_replica_pilot.rs:3968`, `kuberic-operator/src/durable/remove_replica_pilot.rs:4027`, `kuberic-operator/src/durable/remove_replica_pilot.rs:4090` |
| Quarantined exposed reload, restart recovery, terminal reload/compaction, lost-reply/CAS recovery, UID postconditions, admission projection | `kuberic-operator/src/durable/remove_replica_pilot.rs:4144-4498` |

The explicit implementation separately tests operation v2 admission and v1
rejection, exact commit recording, safe compensation, dispatch ambiguity,
bounded attempts, resource-version conflict replay, UID-fenced cleanup,
connection absence, and restart poisoning
(`kuberic-operator/src/durable/remove_replica.rs:1883-3040`). End-to-end
reconciler tests cover the pilot matrix at
`examples/kvstore/tests/reconciler.rs:6546-7279` and explicit/coarse removal
regressions beginning at `examples/kvstore/tests/reconciler.rs:7301`,
`examples/kvstore/tests/reconciler.rs:7428`,
`examples/kvstore/tests/reconciler.rs:7499`,
`examples/kvstore/tests/reconciler.rs:7538`, and
`examples/kvstore/tests/reconciler.rs:8872`.

**Evidence**: Current operator unit tests and high-fidelity kvstore reconciler
tests; the documented matrix summarizes the same fault classes
(`docs/features/kuberic/testing.md:499-544`).

**Implications**: Removing the explicit path does not remove any invariant.
The listed assertions form the traceability baseline for the sole production
runner and compact contracts.

### Question 5: What checkpoint format/version/admission behavior exists, and how are checkpoint classes distinguished?

**Answer**:

- The kernel envelope format is exactly version 3 and rejects unknown envelope
  fields. Loading any other format returns `UnsupportedFormat`; malformed JSON
  returns `InvalidJson`
  (`durable-execution/src/checkpoint.rs:10`,
  `durable-execution/src/checkpoint.rs:42-119`).
- The payload has an explicit tagged lifecycle: `Active` contains activity
  history; `Terminal` contains only terminal outcome and completed activity
  count (`durable-execution/src/checkpoint.rs:154-214`). Terminalization is a
  validated active-to-terminal replacement
  (`durable-execution/src/checkpoint.rs:225-244`).
- Validation distinguishes incompatible execution ID, exact workflow input,
  terminal payload bound, admitted capacity, record count, encoded size,
  non-contiguous/pending history, oversized results, and oversized terminal
  payload (`durable-execution/src/checkpoint.rs:246-273`,
  `durable-execution/src/checkpoint.rs:458-578`).
- Switchover pilot reference/workflow version is 2; remove pilot
  reference/workflow version is 1; remove's underlying CRD operation version is
  2 (`kuberic-operator/src/durable/pilot.rs:58-66`,
  `kuberic-operator/src/durable/remove_replica_pilot.rs:49-57`,
  `kuberic-operator/src/crd.rs:306-308`). Remove admission additionally fixes
  16 records, 770,048 encoded bytes, 4,096 terminal bytes, at most three
  replicas, and projected capacity before execution
  (`kuberic-operator/src/durable/remove_replica_pilot.rs:49-57`,
  `kuberic-operator/src/durable/remove_replica_pilot.rs:1501-1585`).
- Active checkpoints are interpreted from their final activity state;
  terminals are validated and published without another workflow poll
  (`kuberic-operator/src/reconciler.rs:3917-3970`). A dispatch-exposed final
  activity is quarantined on reload
  (`durable-execution/src/host.rs:344-363`). CAS conflict and outcome-unknown
  both require reload before another permit
  (`durable-execution/src/host.rs:848-856`).

**Evidence**: Kernel checkpoint schema/errors, workflow-specific version
checks, and operator reload behavior.

**Implications**: There is no checkpoint migration or compatibility fallback
today. A new production contract needs its own explicit discriminator and must
reject old pilot/explicit shapes unless a deliberate converter is specified;
format or contract mismatch cannot be treated as a fresh execution.

### Question 6: What do the authoritative merged PR #59 measurements report?

**Answer**:

- The lexical script defines non-overlapping scopes and a frozen shared
  baseline (`scripts/measure-switchover-complexity.py:15-165`,
  `scripts/measure-switchover-complexity.py:222-282`). Running it at merged
  `main` produced:
  - explicit remove: **1,611 executable lines / 213 decision points**;
  - complete remove module: **1,632 / 162**;
  - remove integration: **1,114 / 81**;
  - shared growth: **374 / 3**;
  - remove marginal: **3,120 / 246**;
  - marginal ratios: **1.936685 / 1.154930**;
  - shared-growth ratios: **0.309603 / 0.027273**;
  - classification: **negative / negative**.
  The calculation and thresholds are emitted by
  `scripts/measure-switchover-complexity.py:343-482` and protected by unit
  tests (`scripts/tests/test_measure_switchover_complexity.py:19-140`).
- The authoritative fixture extracts terminal bytes from the stored
  checkpoint, activity counts and write counts from runtime measurements, and
  prints all samples (`examples/kvstore/tests/reconciler.rs:7200-7297`).
  A fresh run on this branch (whose production tree matches merged `main`)
  reported:

  | Sample | Effects | Passive | Boundaries | Accepted writes | Active bytes | Terminal bytes | Terminal payload |
  |---|---:|---:|---:|---:|---:|---:|---:|
  | 1 | 3 | 2 | 5 | 6 | 5,013-93,833 | 8,125 | 2,188 |
  | 2 | 3 | 2 | 5 | 6 | 5,021-93,841 | 8,129 | 2,188 |
  | 3 | 3 | 2 | 5 | 6 | 5,017-93,837 | 8,121 | 2,188 |

  Aggregate active range was **5,013-93,841 bytes** and aggregate terminal
  range was **8,121-8,129 bytes**. The test passed.
- Documentation records the stable semantic counts—3 effects, 2 passive
  observations, 5 boundaries, 6 accepted writes—and identifies retained
  history plus repeated full-state activity projections as the dominant active
  payload contributors (`docs/features/kuberic/durable-execution-roadmap.md:234-260`,
  `docs/features/kuberic/testing.md:526-547`). It also states that byte values
  are run-specific snapshots, not exact contracts
  (`docs/features/kuberic/durable-execution-roadmap.md:245-250`).

**Evidence**: Direct execution of the current script and the exact
three-sample fixture on 2026-09-07, plus the merged measurement sources.

**Implications**: The specification can treat semantic counts and configured
bounds as contracts, but not freeze one run's serialized byte counts. Payload
reduction should target repeated activity state/results and retained active
history rather than changing effect or observation boundaries.

### Question 7: Which documentation/examples still expose the pilot comparison, and which thresholds exist only for it?

**Answer**:

- The durable-execution README says both pilots are default-off, explicit is
  the runtime default, and remove did not demonstrate source-cost amortization
  (`durable-execution/README.md:403-420`).
- The roadmap describes both explicit implementations as retained defaults and
  advertises reproducible explicit-versus-pilot source accounting
  (`docs/features/kuberic/durable-execution-roadmap.md:34-50`), then records the
  negative remove comparison and its payload measurements
  (`docs/features/kuberic/durable-execution-roadmap.md:216-260`).
- Testing documentation names “Feature-gated durable-execution
  remove-replica pilot,” says explicit remains default, and publishes pilot-only
  commands (`docs/features/kuberic/testing.md:499-524`). Protocol documentation
  requires a build feature plus per-resource `durablePilot`
  (`docs/features/kuberic/protocols.md:150-170`).
- Deployable/generated surfaces expose `removeReplicaExecutionMode` and
  `durableRemoveReplicaPilot`
  (`kuberic-operator/deploy/deployment.yaml:70-76`,
  `kuberic-operator/deploy/deployment.yaml:132-156`); the kvstore crate exposes
  the pilot feature (`examples/kvstore/Cargo.toml:7-10`). No checked-in
  `KubericSet` example sets `removeReplicaExecutionMode`; omission therefore
  exercises explicit mode.
- The comparison-only classifier says a dimension is positive only when
  marginal ratio is below 1.0 and shared growth is at most 25%; it is negative
  when marginal ratio is at least 1.0 or shared growth exceeds 50%; otherwise
  it is mixed (`scripts/measure-switchover-complexity.py:293-329`). The
  65,536-byte baseline and 32,768-byte stretch gates are documented as
  switchover comparison gates, not remove acceptance gates
  (`docs/features/kuberic/durable-execution-roadmap.md:251-257`).

**Evidence**: Current README, roadmap, testing/protocol docs, generated CRD,
example feature manifest, and measurement script.

**Implications**: Production migration documentation no longer needs
explicit-versus-pilot choice, amortization classification, or switchover-only
comparison thresholds as remove-replica requirements. Stable checkpoint
capacity and terminal bounds remain behavioral constraints.

### Question 8: What conventions govern titles, validation, Kind, images, and owner/retention behavior?

**Answer**:

- No repository document defines a universal commit/PR title format. Recent
  feature commits on `main` commonly use bracketed feature prefixes; this work
  specifically mandates `[Framework-Native Remove Replica]`, local commits on
  the target branch, and exactly one final PR to `main`
  (`.paw/work/framework-native-remove-replica/WorkflowContext.md:41`).
- Standard CI runs `cargo check --all-targets`, formatting, the complexity
  script and its tests, clippy with warnings denied, targeted durable pilot
  gates, `cargo build --all-targets`, a Kind job, image building/loading, and
  finally `cargo test --all --all-features`
  (`.github/workflows/CI.yml:43-89`). The testing guide lists meaningful local,
  doc, core, reconciler, and targeted durable commands
  (`docs/features/kuberic/testing.md:200-220`).
- Kind/image conventions are encoded in `justfile`: cluster name `kind`,
  `deploy/kind-config.yaml`, workspace binary build, images tagged
  `localhost/kuberic-operator` and `localhost/kvstore`, and `kind load
  docker-image ...:latest --name kind` before applying manifests
  (`justfile:1-47`). CI uses `helm/kind-action` with the same cluster/config and
  invokes `just images` (`.github/workflows/CI.yml:78-89`). Live kvstore tests
  are `test_kvstore_k8s_write_read` and `test_kvstore_k8s_status_healthy`
  (`kuberic-tests/src/kvstore_k8s.rs:2-50`).
- Pilot checkpoints are same-namespace ConfigMaps with non-controlling,
  non-blocking owner references; the provider validates namespace and owner
  identity and refuses owner-relationship changes
  (`durable-execution/src/kubernetes.rs:41-118`,
  `durable-execution/src/kubernetes.rs:393-476`). The remove pilot creates that
  owner from the exact `KubericSet` UID
  (`kuberic-operator/src/durable/remove_replica_pilot.rs:1509-1532`).
  Writer RBAC deliberately omits delete
  (`durable-execution/deploy/checkpoint-writer-rbac.yaml:1-25`); a separate
  cleanup identity may list/delete retained orphan checkpoints
  (`durable-execution/deploy/checkpoint-cleanup-rbac.yaml:1-33`). With an owner,
  Kubernetes garbage collection removes the checkpoint after owner deletion;
  the real-API test proves this
  (`durable-execution/tests/kubernetes_checkpoint_real.rs:256-335`).

**Evidence**: Workflow instructions, CI, test guide, just recipes, Kubernetes
provider/RBAC, and real-API owner-GC test.

**Implications**: Validation for this migration should retain the existing
targeted remove matrix plus workspace CI commands. Live Kind validation should
use repository recipes and loaded local images. Checkpoint lifecycle behavior
must remain explicit about owner GC versus independently retained orphan
cleanup.

### Question 9: Which shared abstractions can support a reusable runner, and what add-replica needs remain open?

**Answer**:

- Existing workflow-neutral extension points are:
  `DurableActivity` for versioned bounded input/output
  (`durable-execution/src/typed.rs:52-126`);
  `PreparedActivityResolver` for replay-validated exact dispatch
  (`durable-execution/src/typed.rs:27-50`);
  `HostOutcome`, fused host methods, and exact single-use dispatch permits
  (`durable-execution/src/host.rs:69-164`,
  `durable-execution/src/host.rs:333-410`,
  `durable-execution/src/host.rs:540-674`);
  workflow-keyed host caching/provider construction
  (`kuberic-operator/src/durable/workflow_host.rs:66-155`);
  workflow-supplied measurement decoding
  (`kuberic-operator/src/durable/pilot_store.rs:48-93`); and shared exact
  replica commands plus portable effect outcomes
  (`kuberic-operator/src/durable/effects.rs:45-173`).
- The effect layer explicitly states that it owns no worker or scheduler and
  continues to dispatch through `ReplicaHandle`
  (`kuberic-operator/src/durable/effects.rs:1-6`). Operator documentation also
  states there is no extra worker, queue, lease, watcher, distributed owner, or
  retry scheduler (`docs/features/kuberic/operator.md:289-297`).
- Add-replica currently has distinct behavior that a future port would need to
  express without being hard-coded into the remove runner: operation version
  3; ScaleUp versus Rebuild and optional retirement of an old incarnation;
  one coarse `AddReplicaIntent` to the current primary; a 600-second operation
  deadline plus compensation grace; target prepare/copy/activate; target
  generation-qualified build proof; pre-commit compensation; and
  post-commit clean or degraded publication
  (`kuberic-operator/src/durable/add_replica.rs:20-139`,
  `kuberic-operator/src/durable/add_replica.rs:294-386`,
  `docs/features/kuberic/implemented/agent-owned-replica-add-build-protocol.md:28-106`).
- Add and removal intentionally share lifecycle identity/signature/replay
  primitives but retain separate intents, coordinators, configuration
  semantics, commit rules, and terminal results
  (`docs/features/kuberic/implemented/agent-owned-replica-add-build-protocol.md:53-63`,
  `docs/features/kuberic/implemented/agent-owned-replica-remove-protocol.md:169-179`).

**Evidence**: Current generic kernel/operator APIs and current add-replica
behavior.

**Implications**: The reusable runner boundary can remain an in-process
adapter over the existing controller reconcile cycle. Its extension points
must permit workflow-specific observation collection, preparation/effect
bridging, deadline policy, terminal validation, and publication. No current
behavior requires a queue/worker abstraction.

### Question 10: Does the current ReplicaAgent or gRPC contract prevent compact framework-native removal?

**Answer**: No protocol defect was found.

- `RemoveReplicaIntent` already signs operation/action/attempt identity, mode,
  epoch, exact primary/target identities, generation/control version,
  endpoints, three structural configurations, quorum/minimum constraints, pod
  UID, and deadlines; validation rejects signature or authority drift
  (`kuberic-core/src/remove_replica.rs:18-129`,
  `kuberic-core/src/remove_replica.rs:184-231`).
- Agent observations expose action ID, signature, state, bounded result, and
  typed remove progress; action signatures are deterministic
  (`kuberic-core/src/types.rs:426-496`). Remove progress validates commit time,
  retirement/compensation expiry, current-install dispatch, exact connection
  absence, retirement state, and terminal result consistency
  (`kuberic-core/src/remove_replica.rs:268-397`).
- gRPC status decoding preserves the current and retained action ledger and
  rejects malformed status; correlated execution returns the typed
  acknowledgement observation
  (`kuberic-core/src/grpc/handle.rs:390-493`). The behavioral contract requires
  correlated control protocol 3, exact duplicate replay, and fail-closed
  missing/malformed/unsupported status
  (`docs/features/kuberic/protocols.md:11-56`).
- Unknown checkpoint-write outcomes are intentionally handled above gRPC by
  CAS reload and exposed-activity quarantine
  (`durable-execution/src/host.rs:149-163`,
  `durable-execution/src/host.rs:848-856`). Exact UID label/delete cleanup is
  also an operator/Kubernetes proof, not a missing ReplicaAgent capability
  (`kuberic-operator/src/durable/effects.rs:880-1015`).

**Evidence**: Core remove intent/progress types, agent observation contract,
gRPC handle, kernel reload semantics, and Kubernetes effect adapters.

**Implications**: Compacting framework workflow state and runner plumbing does
not require changing `ReplicaAgent` or gRPC. Protocol versions and exact
signatures remain immutable authority; checkpoint compatibility must be
handled by explicit framework contract discrimination rather than transport
fallback.

## Open Unknowns

None. All internal questions were answerable from merged code, documentation,
tests, local git history, and direct measurement execution.

## User-Provided External Knowledge (Manual Fill)

None.
