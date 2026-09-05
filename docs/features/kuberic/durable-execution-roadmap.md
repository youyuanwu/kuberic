# Durable Execution Framework Roadmap

This document tracks deferred work for the experimental
`kuberic-durable-execution` crate. The current crate is a replay and persistence
safety kernel, not an end-user orchestration runtime. Items below are ordered
possibilities, not commitments.

The ordering is informed by the broader user and provider surfaces in
[Azure Durable Task Framework](https://github.com/Azure/durabletask) and
[Duroxide](https://github.com/microsoft/duroxide). It does not claim API
compatibility with either project.

## Current Boundary

The implemented kernel provides:

- deterministic linear replay;
- exact activity matching and stable logical identity;
- asynchronous load and compare-and-swap persistence;
- opaque provider revision tokens and conservative unknown outcomes;
- separately persisted schedule and dispatch exposure;
- bounded activity count and encoded checkpoint size;
- maximum-result capacity reservation before dispatch;
- ambiguity quarantine and authoritative observation recovery.

The bounds prevent unlimited growth and ensure a declared-valid result remains
persistable after dispatch. They do not compact active history: every completed
activity input and result remains in the checkpoint until the workflow
terminates or reaches a configured limit.

## History Lifecycle

### Next: Completion-Only Compaction

The first lifecycle extension should compact only terminal workflows:

1. Keep complete bounded history while a workflow is active.
2. When replay reaches terminal completion, compare-and-swap the active
   checkpoint to a bounded terminal record.
3. Retain only execution and workflow version identity, terminal outcome,
   bounded result or error summary, completed activity count, and optional
   history digest.
4. Report durable completion only after the terminal transition is accepted or
   subsequently observed.
5. Return the terminal result directly on later loads without replaying the
   discarded history.

The terminal representation needs its own fixed result/error bounds. Those
bounds must be admitted before the first external effect, or the integration
must retain a separately bounded domain summary instead of an arbitrary
workflow result.

### No Generic Mid-Operation Compaction

Mid-operation compaction is not planned for the initial Kuberic pilot.
Deterministic replay may depend on any previous activity result, so deleting an
active prefix requires a new durable continuation state and changes the
workflow authoring contract. The framework must not silently discard history
or infer a continuation point from checkpoint pressure.

When an active workflow reaches an activity-count or encoded-size limit, it
fails before scheduling another effect. It does not automatically compact or
start a replacement execution.

### Deferred: Continue-as-New

If a concrete long-running workflow cannot fit within the bounded active
history, evaluate an explicit Continue-as-New operation:

1. The workflow requests continuation with new bounded input.
2. Storage atomically terminates the current execution and creates a fresh
   execution checkpoint.
3. The new execution starts with empty history and a distinct execution
   identity.
4. The terminal record may retain a link between the old and new executions.
5. Recovery never starts a continuation merely because a size limit is near.

Continue-as-New should be added only after a real workflow demonstrates the
need. Short topology workflows should complete and use terminal compaction.

## Deferred Usability Work

### Kernel Ergonomics

1. Add typed serde workflow and activity adapters while retaining exact
   encoded-byte matching.
2. Add durable activity failure results and deterministic failure replay.
3. Add an activity registry and dispatch adapter.
4. Add a passive convergence resolver for quarantined activities.
5. Add replay-aware tracing and checkpoint inspection.

### Workflow Primitives

Add durable timers and retry policy only when a concrete Kuberic workflow
requires them. Retry behavior must not turn an uncertain exposed activity into
an automatic duplicate dispatch.

External events, parallel scheduling, join/select, child workflows, and
cancellation remain deferred until demonstrated by a specific workflow.

### Runtime and Operations

Generic instance lifecycle and query APIs, workers, queues, leases, routing,
and distributed ownership are not required for the first operator-hosted
pilot. The Kubernetes operator already supplies reconciliation wakeups and
effect ownership.

If the framework later serves applications outside the operator, reassess
those runtime facilities rather than growing the kernel speculatively.

### Kubernetes Integration

After the provider contract and history lifecycle are proven:

1. Implement a Kubernetes checkpoint provider using opaque
   `resourceVersion` compare-and-swap.
2. Measure checkpoint size, accepted writes, and watch-delivered bytes.
3. Add one operator workflow pilot without changing workflow ownership.
4. Evaluate whether the async authoring model is materially simpler than the
   existing explicit state machine.
5. Generalize only if the pilot preserves safety and reduces complexity.

## Explicitly Deferred

The roadmap does not currently commit to:

- production migration or mixed-version checkpoint support;
- exactly-once activity execution;
- generic automatic compensation;
- worker queues, leases, or a distributed scheduler;
- a public orchestration platform;
- adoption by existing Kuberic workflows.
