# Framework-Native Remove Replica

## Overview

Remove-replica is now a production framework-native durable workflow. Every
accepted removal uses one versioned execution reference,
`status.removeReplicaExecution`, and the shared in-process durable runner.
There is no remove execution-mode field, Cargo feature, comparison path, or
explicit remove state machine.

The migration preserves the existing `ReplicaAgent` protocol and the
agent-owned remove sequence. It changes how the operator persists, recovers,
and publishes that protocol: immutable admission is stored once, each durable
boundary contains only an exact command or compact evidence, uncertain effects
are recovered from authoritative state, and topology is published only after a
terminal checkpoint has been accepted and reloaded.

The implementation supports the existing maximum three-member remove domain.
It does not migrate add-replica and does not add a worker, queue, lease,
watcher, scheduler, or separate durable service.

## Architecture and Design

### High-Level Architecture

The production path has four layers:

1. The reconciler admits an eligible `ScaleDown` or internally authorized
   `Force` removal and persists the immutable execution reference in
   `KubericSet` status.
2. The shared operator runner loads the authoritative checkpoint, enforces
   bounded progress, handles common persistence/recovery outcomes, and gives an
   operation adapter a one-use permit only for an accepted prepared effect.
3. The remove adapter reconstructs operation state from immutable admission
   plus compact boundary history, collects current Kubernetes and
   `ReplicaAgent` evidence, validates authority, and resolves or dispatches the
   exact prepared command.
4. The existing agent-owned remove protocol commits the reduced topology,
   removes the exact old-incarnation connection, retires the target when
   possible, and returns evidence used for UID-fenced Kubernetes cleanup.

Kubernetes reconciliation remains the scheduler and wakeup source. The runner
is a Rust component inside `kuberic-operator`, while
`kuberic-durable-execution` remains a workflow-neutral replay and persistence
kernel.

### Shared Runner Contract

The shared runner owns the lifecycle behavior used by switchover and
remove-replica:

- load and validate the authoritative execution record;
- return a loaded terminal without polling replicas;
- bound each reconcile by host-outcome fuel and a deadline-clamped requeue;
- expose an exact prepared command before yielding a one-use dispatch permit;
- fuse an authoritative observation with progression to the next command or
  terminal where current evidence is sufficient;
- quarantine an unresolved exposed command and require authoritative
  resolution;
- reload after a compare-and-swap conflict or outcome-unknown write;
- stop on definite persistence failure or nondeterminism; and
- validate a terminal through the operation adapter before handing it to
  publication.

The common outcome vocabulary is:

| Outcome | Meaning |
|---|---|
| Active | More work or fresh authority is required; return a bounded requeue. |
| Terminal | A terminal record was accepted and then authoritatively reloaded. |
| Incompatible | The execution/checkpoint contract cannot be interpreted by this version. |
| Rejected | The record or operation violates a current contract requirement. |
| Isolated | Evidence, ownership, observation, or adapter behavior is unsafe to continue. |
| Conflicted | A compare-and-swap conflict requires authoritative reload before progression. |
| Unknown write | Persistence may or may not have applied; reload before any later permit. |
| Persistence failure | A definite load or write failure stopped the cycle. |
| Nondeterministic | Replay requested a boundary inconsistent with durable history. |

Conflict and unknown-write cases are represented by the runner's
`ReloadRequired` result with a typed reload reason. Neither case, nor any
definite persistence failure, grants dispatch authority.

### Six Operation-Specific Responsibilities

The shared runner deliberately does not understand replica topology. Each
operation adapter supplies six independently testable responsibilities:

1. **Observation collection** — obtain the exact replica, pod, label,
   generation, epoch, configuration, and lifecycle evidence needed by the
   operation.
2. **Authority and preparation** — validate current authority and derive the
   exact immutable command for the next durable boundary.
3. **Effect dispatch and quarantine interpretation** — consume the one-use
   permit, call the existing effect boundary, and decide whether authoritative
   evidence completes, waits, isolates, or permits bounded redelivery.
4. **Deadline policy** — select the operation deadline used to clamp reconcile
   requeues and classify waits.
5. **Terminal validation** — bind the loaded terminal to immutable admission,
   exact commit/cleanup evidence, and completed-boundary accounting.
6. **Publication and conditions** — translate a validated terminal into
   operation-specific status, topology, and condition changes.

The named remove tests for these responsibilities are recorded in
[`SafetyTraceability.md`](SafetyTraceability.md).

### Compact Remove Contract

The current serialized remove contract version is **3**. Version 3 changed the
prepared `ReplicaAgent` action payload from hexadecimal text to binary
`ExactBytes`, reducing the representative prepared command while preserving
the same protobuf action and protocol signature. Boundary activity identity is
version 2.

Immutable admission contains the execution and operation identities, accepted
mode, previous stable snapshot, exact target identity/UID/address/generation,
minimum committed replicas, and accepted/overall-deadline times. The reduced
topology is derived deterministically rather than persisted beside the
previous snapshot.

Boundary inputs are tagged as a passive observation, exact replica command,
exact UID-fenced label command, or exact UID-fenced delete command. Boundary
results contain compact observation evidence, a typed effect result, or
bounded proven-no-admission redelivery evidence. They never contain the
complete mutable operation state or a complete repeated multi-configuration
snapshot.

Terminal records are compact typed `Completed`, `Compensated`, `Unsafe`,
`Rejected`, or `IncompatibleContract` outcomes. A completed terminal stores
commit evidence, cleanup proof, and activity accounting once; immutable
admission remains the authority for fields that need not be duplicated.

### Compatibility and Clean Break

Contract version, workflow input, checkpoint limits, execution identity,
checkpoint name, terminal payload bound, and persisted owner relationship are
validated as one immutable contract.

- Native contract versions other than 3 are incompatible.
- Legacy remove-pilot workflow inputs and references are incompatible.
- Legacy explicit remove status is incompatible.
- On first reconciliation, a legacy status shape is replaced atomically by a
  durable `status.removeReplicaExecution` incompatibility marker containing
  source kind, legacy version, execution identity, and stable fingerprint.
- An incompatibility marker is never treated as an absent execution and never
  authorizes fresh admission.
- Malformed, oversized, wrong-execution, wrong-owner, and unsafe evidence
  remain rejected or isolated rather than being mislabeled as compatible.

There is no converter, mixed-version execution, fallback path, or silent
restart under the current contract.

### Safety and Publication

The retained safety matrix maps all 34 deletion-gate invariants to named
framework-native, shared-runner, provider, or live-cluster tests. It covers
immutable mode, every-boundary restart, exact prepared commands, one-use
dispatch, lost replies, conflict/unknown-write reload, authority and fencing,
commit and cleanup evidence, bounded redelivery, malformed/incompatible
records, terminal ordering, admission bounds, ownership, owner garbage
collection, and separately authorized cleanup.

Exact reduced `Current` is the irreversible membership commit. Before commit,
the domain may continue safely or compensate. After commit, it can only roll
forward. Completion requires exact connection absence, terminal target
retirement evidence or its allowed degraded form, UID-fenced label cleanup,
and UID-fenced deletion or authoritative absence of the admitted UID.

The terminal checkpoint is persisted first. The runner then reloads it,
validates it against immutable admission and completed-boundary accounting,
and only then returns the publication handoff. The reconciler cannot publish
the reduced `stableSnapshot` from an accepted-but-not-reloaded terminal.
Publication conflict leaves the terminal checkpoint available for a later
status-only retry without redispatching effects.

### Checkpoint Ownership and Retention

Each execution uses a same-namespace ConfigMap named
`kuberic-checkpoint-<execution-id>`. The checkpoint has a non-controlling,
non-blocking owner reference to the exact `KubericSet` name and UID. Loads and
replacements reject a changed owner relationship.

Active history is replaced by a compact terminal record; the provider does not
delete the terminal as part of terminalization. The operator writer requires
ConfigMap `get`, `create`, and `update`, not delete. Owner deletion allows
ordinary Kubernetes garbage collection to remove the checkpoint. Explicit
orphan cleanup is a separate lifecycle responsibility with independently
scoped `list` and `delete` RBAC; it must apply its own retention and recovery
safety policy.

## User Guide

### Prerequisites

- A healthy `KubericSet` with three stable members for the current production
  remove domain.
- A non-primary removal target selected by the operator.
- Retained membership satisfying `minReplicas` and previous write-quorum
  safety.
- The existing correlated control v3, `RemoveReplicaIntent` v1, and
  `ReplicaLifecyclePeer` v2 protocol support.
- ConfigMap checkpoint writer permissions in the `KubericSet` namespace.

### Basic Usage

Request scale-down by changing only the desired replica count:

```yaml
spec:
  replicas: 2
```

No remove execution-mode selector is accepted or required. Healthy scale-down
uses `ScaleDown`; stale/dead/missing-secondary health recovery may authorize
`Force`. These are immutable domain safety modes chosen by admission, not
alternative execution engines.

Inspect `status.removeReplicaExecution` for the immutable execution reference
and the remove condition for active, incompatible, rejected, isolated,
reload-required, persistence-failure, nondeterministic, unsafe, compensated,
or completed state.

### Recovery Behavior

- A restart reloads the checkpoint and replays compact history.
- A loaded terminal publishes without polling or dispatching.
- An exposed effect with no accepted result remains quarantined until exact
  observation resolves it.
- A conflict or unknown write reloads before any later dispatch permit.
- A temporary exact primary/target status gap waits without operation
  generation churn.
- Same-name replacement pods are protected by the admitted UID fence.

## API Reference

### Production Execution Constants

| Contract property | Value |
|---|---:|
| Remove contract version | 3 |
| Maximum members | 3 |
| Maximum history records | 16 |
| Maximum immutable workflow input | 2,048 bytes |
| Maximum boundary input | 4,096 bytes |
| Maximum boundary result | 2,048 bytes |
| Maximum active encoded record | 262,144 bytes |
| Maximum terminal encoded record | 12,288 bytes |
| Maximum terminal payload | 4,096 bytes |
| Maximum deterministic transition fuel | 48 |

Changing the lifecycle limits without changing the contract version is
incompatible.

### Representative No-Fault Semantics

The canonical three-member `ScaleDown` path is exactly:

- **3 external effects**: one correlated `RemoveReplicaIntent`, one UID-fenced
  label update, and one UID-fenced pod deletion;
- **2 passive observations**;
- **5 completed durable boundaries**; and
- **6 accepted writes**, consisting of the initial accepted exposure, fused
  observation/progression writes, and terminal persistence.

External effects, passive observations, completed boundaries, and accepted
writes are separate measurements. A write attempt is not counted as accepted
unless persistence returns an authoritative revision.

### Byte Measurements and Bounds

The final three-sample no-fault run observed:

| Measurement | Run-specific value |
|---|---:|
| Active record lifecycle range | 3,373–18,693 bytes |
| Per-run active maxima | 18,685; 18,685; 18,693 bytes |
| Terminal record | 4,245 bytes |
| Terminal payload | 683 bytes |

These byte values are measurements, not compatibility constants. Every
representative active maximum remains below the **49,152-byte (48 KiB)**
acceptance gate.

The maximum-fault projection uses 16 records, 4,096-byte inputs, and
2,048-byte results. It produced a 182,589-byte active record, an 11,453-byte
terminal record, and a 4,096-byte terminal payload, each independently within
the 262,144/12,288/4,096 lifecycle limits. One-byte-over tests reject record
17, input 4,097, result 2,049, active 262,145, terminal 12,289, and terminal
payload 4,097.

## Testing

### Targeted Local Gates

```console
cargo test -p kuberic-operator framework_native_remove_replica
cargo test -p kuberic-operator durable_runner_tests
cargo test -p kuberic-operator checkpoint_store
cargo test -p kuberic-operator remove_replica_execution_rejects_all_six_one_byte_over_bounds
cargo test -p kuberic-operator remove_replica_execution_maximum_fault_history_and_terminal_fit_independent_bounds -- --nocapture
cargo test -p kvstore --test reconciler test_framework_native_remove_replica_
cargo test -p kvstore --test reconciler test_framework_native_remove_replica_three_no_fault_measurement_samples -- --nocapture
```

### Repository and Live Gates

```console
cargo check --all-targets
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo build --all-targets
cargo test -p kuberic-core -p kuberic-operator -p kvstore -p sqlite-replicated
cargo test --doc --workspace
python3 scripts/measure-switchover-complexity.py
python3 -m unittest scripts.tests.test_measure_switchover_complexity
KUBECONFIG="$HOME/.kube/kuberic-kind-config" cargo test --all --all-features
```

The isolated Kind sequence additionally runs `just images`, the real
Kubernetes checkpoint provider test, the live status and read/write tests, and
`test_kvstore_k8s_framework_native_remove_replica`. The live remove test
verifies no selector is present, removes one exact pod, preserves the two
admitted surviving UIDs, observes the terminal checkpoint, and restores the
shared fixture to three healthy replicas.

### Edge Cases

The retained matrix covers every durable boundary before and after commit,
lost effect replies, unknown applied/unapplied writes, conflicts, malformed
agent status, exact primary/target gaps, generation and configuration drift,
same-name UID replacement, Force without a live target, exhausted redelivery,
oversized records, incompatible versions, publication conflict, owner
garbage collection, and retained-record cleanup authorization.

## Limitations and Future Work

- The production remove contract supports at most three members.
- Legacy execution records are rejected rather than migrated.
- Poisoned ambiguous commit states require an external administrative
  recovery protocol, which is not implemented.
- Generic workers, queues, leases, timers, retries, distributed ownership,
  and automatic active-history compaction remain out of scope.
- Add-replica remains on its existing durable operation and agent-owned
  protocol. A future framework-native add migration should reuse the shared
  runner and provide a new adapter for the same six responsibilities:
  observation collection, authority/exact command preparation, effect and
  quarantine handling, deadline policy, terminal validation, and
  publication/conditions. It should declare its own compact versioned
  admission, boundary, terminal, and lifecycle limits. No additional service
  is required to make that extension.
