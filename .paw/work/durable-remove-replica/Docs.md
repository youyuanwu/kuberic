# Durable Remove Replica

## Overview

This work adds remove-replica as the second workflow hosted by the experimental
durable-execution kernel. The new path is protected by the default-off
`durable-remove-replica-pilot` Cargo feature and an explicit runtime selection.
Omitting the selection continues to use the existing CRD-status remove-replica
state machine.

The port preserves the existing coarse `RemoveReplicaIntent` protocol and uses
the existing explicit decision engine as its semantic authority. It also
provides the intended experiment: whether the shared durable-execution
infrastructure reduces the marginal source complexity of a second substantial
workflow. The measured answer is negative.

## Architecture and Design

### High-Level Architecture

The durable path has four layers:

1. The existing explicit remove-replica decision engine defines admission,
   authority, commit, cleanup, and terminal semantics.
2. The remove-replica workflow module projects that operation into bounded
   typed activities and compact terminal outcomes.
3. Shared operator hosting provides execution-keyed checkpoint stores,
   measurement decoding, host caching, and single-use dispatch permits.
4. The reconciler owns observations, effect dispatch, quarantine recovery, and
   final CRD publication.

The checkpoint is the replay authority for the kernel execution. CRD status
first stores the immutable execution reference; the first checkpoint is created
on a later reconcile. Final topology/status publication occurs only after a
terminal checkpoint has been accepted or authoritatively reloaded.

### Design Decisions

- **Explicit behavior remains the default.** The durable path requires both
  compiled feature support and explicit runtime selection. Unsupported
  selection fails rather than silently falling back.
- **The existing protocol remains authoritative.** The port does not redefine
  ReplicaAgent or gRPC semantics. Exact primary generation, control version,
  epoch, configuration, target incarnation, and Pod UID remain fenced.
- **Effects are exposed before dispatch.** Exact correlated replica commands,
  exact-UID role changes, and exact-UID deletion commands are durably accepted
  before a matching permit can authorize dispatch.
- **Unknown outcomes remain conservative.** Unknown persistence or effect
  outcomes reload authoritative state. An exposed effect without sufficient
  evidence is quarantined rather than duplicated or inferred complete.
- **Commit precedes cleanup.** Reduced-Current commit evidence is durable before
  retirement, labeling, deletion, or stable-topology publication.
- **Terminal precedes status.** Compact terminal persistence precedes final CRD
  publication, allowing a restarted operator to finish publication without
  polling the workflow again.

### Integration Points

- `kuberic-operator/src/crd.rs` defines the default-explicit selection and
  durable execution reference.
- `kuberic-operator/src/durable/remove_replica_pilot.rs` defines the bounded
  workflow, prepared activities, admission, replay, and terminal payloads.
- `kuberic-operator/src/durable/effects.rs` supplies exact commands and
  remove-specific UID-fenced label/delete integration.
- `kuberic-operator/src/durable/workflow_host.rs` supplies the shared host,
  checkpoint provider, cache, and permit guard.
- `kuberic-operator/src/reconciler.rs` accepts, advances, observes, dispatches,
  recovers, and publishes the operation.
- `scripts/measure-switchover-complexity.py` measures non-overlapping workflow,
  shared, and integration scopes.

## User Guide

### Prerequisites

- Build the operator with `durable-remove-replica-pilot`.
- Select the durable remove-replica execution mode on the target `KubericSet`.
- Use an admitted set with no more than three stable members.
- Provide the same healthy ScaleDown or fenced Force conditions required by the
  explicit remove-replica workflow.

### Basic Usage

Without explicit durable selection, removal follows the existing explicit
state machine. With feature support and durable selection, the operator writes
an immutable execution reference, creates an owner-bound ConfigMap checkpoint,
and advances the kernel execution across reconciles.

The operation remains observable through the normal Kuberic status surface.
Completion publishes the reduced stable topology only after terminal
checkpoint acceptance and required cleanup evidence.

### Advanced and Failure Behavior

- A lost replica-effect reply remains exposed until correlated agent evidence,
  an exact postcondition, or proof of non-admission resolves it.
- A changed primary generation can redrive only from the explicit workflow's
  safe pre-commit configurations and within its existing attempt bound.
- Ambiguity after possible reduced-Current installation cannot restore the
  previous topology. It terminates as an unsafe typed disposition.
- Exact-UID cleanup treats replacement Pods as different resources and never
  mutates or deletes them under the old execution authority.
- A terminal checkpoint can be reloaded after process restart or final status
  write failure without re-polling workflow code.

### Safety Mapping

| Safety contract | As-built behavior |
|---|---|
| Admission and quorum | The durable path accepts only the same non-primary stable secondary, retained minimum, and previous-write-quorum conditions as the explicit operation. |
| Primary authority | Prepared replica commands bind the exact primary incarnation, generation, control version, runtime epoch, protocol, and frozen configuration identity. |
| ScaleDown versus Force | ScaleDown retains target lifecycle authority and exact generation requirements; Force permits missing target authority without relaxing primary, topology, quorum, epoch, or UID fences. |
| Unknown effect outcome | The next reconcile reloads checkpoint and agent/cluster authority; it does not infer admission or issue a second permit. |
| Irreversible commit | Reduced-Current commit evidence is persisted before any cleanup and cannot authorize restoration of previous Current after ambiguous dispatch. |
| UID-fenced cleanup | Retired-label and deletion commands bind the admitted Pod name and UID; replacement Pods are not mutated. |
| Unsafe ambiguity | Unresolved post-dispatch ambiguity terminates with the explicit workflow's typed poisoned disposition. |
| Terminal ordering | The compact terminal checkpoint is accepted or reloaded before final topology and operation status publication. |
| Replay | Tests reconstruct fresh hosts across accepted schedule, exposure, observation, commit, cleanup, and terminal boundaries using accumulated checkpoint history. |

## Configuration and Contracts

- Cargo feature: `durable-remove-replica-pilot` (default off)
- Runtime mode default: explicit
- Maximum admitted replicas: 3
- Stable encoded-checkpoint ceiling: 770,048 bytes
- Stable terminal-payload ceiling: 4,096 bytes

Observed checkpoint sizes are not compatibility contracts. Three representative
successful no-fault three-member ScaleDown executions recorded:

- 3 external effects
- 2 passive observations
- 5 completed durable boundaries
- 11 accepted checkpoint writes
- 5,009–101,633-byte maximum active checkpoints
- 8,121–8,125-byte terminal checkpoints
- 2,188-byte terminal payloads

## Source-Cost Result

The port did not demonstrate shared-infrastructure source-cost amortization.
The fixed measurement scopes report:

| Scope | Executable lines | Decision points |
|---|---:|---:|
| Explicit remove baseline | 1,627 | 219 |
| Complete kernel remove workflow | 1,611 | 155 |
| Remove-specific operator integration | 1,086 | 82 |
| Shared-infrastructure growth | 374 | 3 |
| Total marginal cost | 3,071 | 240 |

The marginal ratios are 1.8875 times the explicit implementation in executable
lines and 1.0959 times in decision points. Shared growth is 30.96% in lines and
2.73% in decisions. Both dimensions classify as negative.

The isolated async workflow body is only 138 lines / 18 decisions, but that
scope is not representative of the full cost. Workflow-specific state,
admission, replay, terminal support, effect recovery, routing, and publication
integration eliminate the local body advantage.

## Testing

Run the focused kernel and operator coverage:

```console
cargo test -p kuberic-durable-execution
cargo test -p kuberic-durable-execution --features kubernetes
cargo test -p kuberic-operator --features durable-remove-replica-pilot remove_replica_pilot_
CARGO_BUILD_JOBS=2 CARGO_INCREMENTAL=0 RUST_MIN_STACK=4194304 cargo test \
  -p kvstore --features durable-remove-replica-pilot --test reconciler \
  test_durable_execution_remove_replica_pilot_
python3 scripts/measure-switchover-complexity.py
```

The feasibility evidence is recomputed with:

```console
CARGO_BUILD_JOBS=2 cargo test -p kuberic-durable-execution \
  --test feasibility -- --nocapture
```

The corrected runtime-neutrality predicate checks only root library
dependencies. A runtime used only by tests passes; a real async runtime in the
library dependency table fails. With the current manifest and complete
evidence registry, the mechanical classification is `feasible`.

### Validation Results

The following completed successfully on the final Phase 5 checkout:

- formatting, all-target check/build, and all-feature clippy with warnings
  denied;
- durable-execution default and Kubernetes-feature suites, including the
  authorized real-API checkpoint test;
- the feasibility suite, with all FR-012 and revision predicates passing and
  classification `feasible`;
- 199 combined operator tests;
- 11 durable switchover reconciler tests;
- 17 durable remove-replica reconciler tests;
- explicit add, remove, and failover lifecycle regressions;
- 40 replica-agent and 6 replica-lifecycle tests in the final focused run;
- complexity measurement and its 12 standard-library Python tests.

The workspace-wide `cargo test --all --all-features` command reached the two
live-cluster KV tests, but both
`kuberic-tests::kvstore_k8s::test_kvstore_k8s_write_read` and
`kuberic-tests::kvstore_k8s::test_kvstore_k8s_status_healthy` failed because
the local environment could not execute `kubectl` (`No such file or
directory`). This is an environment exception rather than a code-test failure.
No cluster or deployment was modified to work around it.

## Limitations

- The path remains experimental and default-off.
- The explicit remove-replica implementation remains the production default.
- The active checkpoint retains complete bounded history until terminal
  compaction.
- This work does not authorize more workflow ports or broader migration.
- The negative source-cost result is specific to the measured implementation
  and fixed lexical metric; it does not negate the demonstrated replay,
  persistence, fencing, or checkpoint behavior.
