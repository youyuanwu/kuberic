# Framework-Native Switchover Graduation

## Overview

Switchover now has one production execution path. Every accepted primary
change is admitted into the durable execution framework, persisted through an
owner-bound ConfigMap checkpoint, and advanced by ordinary Kubernetes
reconciliation. The public execution-mode selector, optional build feature,
explicit production route, pilot status/condition, and duplicate test and CI
surfaces were removed.

The migration preserves the existing switchover safety sequence: revoke old
primary writes, capture the frozen LSN, prove target catch-up, demote the old
primary, promote the target, converge retained-member epochs, install CatchUp
and Current configurations, update exact-UID routing labels, attest the final
topology, and only then publish the stable snapshot.

## Architecture and Design

### High-Level Architecture

`status.switchoverExecution` is the durable admission surface. It stores the
contract version, execution ID, deterministic checkpoint name, and either:

- immutable admitted input containing the set authority, previous stable
  snapshot, target primary, and acceptance time; or
- typed incompatibility evidence containing the historical source, version,
  identity, optional checkpoint name, and stable fingerprint.

The referenced same-namespace ConfigMap contains the format-3 active or
terminal checkpoint. The shared in-process runner owns authoritative load,
terminal short-circuit, fused schedule/exposure and
observation/progression, one-use permits, quarantine, persistence outcome
classification, bounded fuel, and deadline-clamped requeue. The switchover
adapter owns replica and Pod observation, exact command preparation,
operation-specific effect recovery, terminal validation, conditions, and
topology publication.

### Design Decisions

#### Retain individually correlated local actions

No coarse agent-owned switchover intent was introduced. The existing sequence
addresses the old primary, target, retained replicas, and exact-UID Kubernetes
routing objects. Each mutation already has deterministic identity, exact
incarnation and epoch authority, process-generation and control-version
fences, and an authoritative postcondition. Moving the full sequence behind
one local intent would require a new cross-agent coordinator without improving
the established ambiguity boundary.

#### Fail closed for historical state

Legacy explicit version-1 and pilot version-1/version-2 status are converted
before pod observation into durable incompatibility evidence. Unsupported or
malformed historical status uses the same blocked surface. Historical state
is never resumed under the new contract, cleared as absent, or used to
authorize another execution.

#### Separate operation-specific bounds

Switchover declares independent limits rather than copying remove-replica:

| Dimension | Limit |
|---|---:|
| Stable members | 3 |
| Activity records | 32 |
| Workflow transitions | 64 |
| Runner outcomes per reconcile | 32 |
| Workflow input | 4,096 bytes |
| Activity input | 8,192 bytes |
| Activity result | 4,096 bytes |
| Active checkpoint | 770,048 bytes |
| Terminal checkpoint | 16,384 bytes |
| Terminal payload | 4,096 bytes |
| Error text | 512 UTF-8 bytes |

Exact one-unit-over tests cover every dimension. Maximum encoded fixtures
measure 714,105 active bytes and 15,093 terminal bytes. The 19-record
success-with-redelivery projection is 427,133 bytes and the 21-record
rollback-with-redelivery projection is 471,285 bytes.

### Integration Points

- The Kubernetes controller remains the scheduler.
- The shared durable runtime and ConfigMap provider are also used by
  framework-native remove-replica.
- Replica mutations continue through correlated control v3 and the existing
  ReplicaAgent ledger.
- Routing mutations continue through exact Pod UID preconditions.
- Add/build, failover, initial creation, and remove-replica retain their
  existing operation-specific behavior.

## User Guide

### Prerequisites

Deploy the ordinary operator image and CRD. No switchover-specific feature or
resource field is required. The operator service account needs ConfigMap
`get`, `create`, and `update`; it does not require delete.

### Basic Usage

Request a new primary through the existing `targetPrimary` status-driven
operator workflow. Once accepted, inspect:

- `status.switchoverExecution` for admission/checkpoint identity or
  incompatibility evidence;
- the `FrameworkNativeSwitchover` condition for active, reload, storage,
  quarantine, blocked, compensated, or completed state;
- the referenced checkpoint ConfigMap for retained durable evidence.

### Recovery Behavior

Operator restart does not restart the operation. Reconciliation reloads the
same checkpoint and exact execution contract. An exposed effect is resolved
from the matching agent ledger entry or live postcondition. A complete process
generation change plus exact precondition may prove non-admission and permit
one bounded redelivery; all other uncertainty remains quarantined.

Terminal state is persisted before CR status publication. If publication
fails, a later reconcile reloads and validates the terminal checkpoint without
polling replicas or redispatching effects.

## API Reference

### Status

`status.switchoverExecution.state.kind` is `admitted` or `incompatible`.
Admitted state includes `input`; incompatible state includes
`incompatibility`. The generated CRD does not expose
`switchoverExecutionMode` or `durableSwitchoverPilot`.

### Conditions

`FrameworkNativeSwitchover` is false for `Completed` and
`CompensatedOrSafeFailure`; active, blocked, reload, persistence,
incompatibility, rejection, isolation, nondeterminism, fuel, and quarantine
states are true.

## Testing

### Targeted Commands

```console
cargo test -p kuberic-operator framework_native_switchover
cargo test -p kuberic-operator \
  framework_native_switchover_rejects_every_independent_one_over_bound
CARGO_BUILD_JOBS=2 CARGO_INCREMENTAL=0 RUST_MIN_STACK=4194304 \
  cargo test -p kvstore --test reconciler \
  test_framework_native_switchover_ -- --nocapture
cargo test -p kuberic-durable-execution --features kubernetes \
  --test kubernetes_checkpoint
```

### Isolated KinD Validation

All KinD validation must create a new workflow-specific cluster and kubeconfig.
Set `KIND_CLUSTER_NAME` and `KUBECONFIG`; the `just` recipes reject the default
`kind` name and default user kubeconfig, verify the exact
`kind-${KIND_CLUSTER_NAME}` context before Kubernetes mutations, and pass the
isolated cluster name to every image load. `KIND_CONFIG` defaults to
`deploy/kind-isolated-config.yaml`, whose kvstore host port is allocated
dynamically; the test resolves the mapping only from the exact dedicated node
container. An ownership receipt binds the cluster name, context, and
kubeconfig before local cleanup is allowed. Cargo real-API tests construct
clients from the explicit kubeconfig and context rather than inherited
defaults.

The final validation used the newly created `kuberic-fns-iso-20260908` cluster
with its workflow-local kubeconfig and ownership receipt. No existing cluster
or container was inspected, reconfigured, stopped, or reused. Image loading,
the real-API provider test, and all five enabled KinD integration tests
including the dynamically mapped kvstore write/read path passed. After the
full workspace attempt, the exact dedicated cluster, kubeconfig, and receipt
were deleted.

The canonical three-member measurement is:

- 9 external effects;
- 3 passive observations;
- 12 completed durable boundaries;
- 13 accepted checkpoint writes;
- 31,785 maximum active-checkpoint bytes;
- 4,169 terminal-checkpoint bytes;
- 1,041 terminal-payload bytes.

Run-specific byte values are measurements, not compatibility constants.

## Edge Cases

- Already-primary and non-member targets are rejected at admission.
- Changed Pod UID, agent generation, control version, runtime epoch, or exact
  action payload prevents stale dispatch.
- Catch-up timeout restores the previous configuration before promotion.
- Failed or uncertain promotion compensates through the new epoch and
  publishes only after full attestation.
- Unknown label or replica effects remain quarantined.
- Conflict and unknown persistence outcomes reload authoritative state before
  another permit.
- Unsupported envelope, contract, activity, terminal, checkpoint identity, or
  status shape fails closed.

## Limitations and Future Work

- The production contract supports at most three stable members.
- Mid-operation compaction and Continue-as-New are not used.
- Add-replica, failover, and initial creation were not migrated.
- SQLite issue #42 was not addressed.
- No worker, queue, lease, watcher, separate durable service, or generic
  orchestration platform was added.
- Real-API checkpoint validation passed against the local Kind cluster. The
  aggregate all-features command still requires locally installed PostgreSQL
  server binaries for the unrelated PostgreSQL example tests.
