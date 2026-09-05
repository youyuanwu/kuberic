# Kuberic Durable Execution Experiment

`kuberic-durable-execution` is an isolated feasibility crate for deterministic,
linear workflow replay. It has no dependency on `kuberic-core` or
`kuberic-operator`, and neither production crate depends on it.

## Selected authoring surface

The feasibility evaluation selected the ordinary async surface. A workflow is
an `#[async_trait(?Send)]` implementation of `Workflow::run`; its only
framework-specific workflow-body operation is
`WorkflowContext::activity(name, input).await`.

```rust
use async_trait::async_trait;
use kuberic_durable_execution::{
    ActivityName, ExactBytes, Workflow, WorkflowContext,
};

struct Greeting;

#[async_trait(?Send)]
impl Workflow for Greeting {
    async fn run(
        &self,
        context: &mut WorkflowContext<'_>,
        input: ExactBytes,
    ) -> ExactBytes {
        context
            .activity(ActivityName::new("greeting", 1).unwrap(), input)
            .await
    }
}
```

The measured workflow body uses one framework operation against an FR-012
maximum of two. The public host reports nine turn/observation outcome variants.
No explicit poll/replay authoring surface is also exported.

## Replay and checkpoint semantics

`ExactBytes` are compared and persisted without normalization. Workflow
history is a contiguous, zero-based sequence with a completed prefix and at
most one final pending activity. A requested activity must match the recorded
sequence, immutable positive-versioned name, and exact input; a mismatch is
nondeterminism rather than a new dispatch.

Format version 1 stores JSON payload bytes in a versioned
`CheckpointEnvelope`. The payload contains execution identity, exact workflow
input, and linear activity records in `Scheduled`, `DispatchExposed`, or
`Completed` state. Envelope version and payload shape are validated before the
workflow is polled. Unsupported versions are compatibility errors, not storage
conflicts. Completed results replay into workflow code and do not create a new
dispatch permit.

Logical activity identity is the complete tuple of execution ID, sequence,
versioned activity name, and exact activity input. Its external rendering
contains the complete tuple without hashing or normalization. `AttemptId` is a
separate host-epoch and monotonic-counter identity, so a discarded
pre-exposure attempt may change without changing the logical activity.

## Storage revisions

`CheckpointStore` exposes synchronous `load` and `compare_and_swap`
operations. `StorageRevision` is an opaque concurrency token and is unrelated
to checkpoint format version. The included `InMemoryCheckpointStore` assigns
the first accepted checkpoint a nonzero revision and advances it once for each
accepted replacement. Its one-shot faults model rejection before apply and
response loss after apply.

A conflict, pre-apply rejection, or lost response returns `ReloadRequired`.
None of those responses is dispatch permission, even when a lost response
means the state was applied.

## Turns and dispatch permission

`DurableHost::turn` evaluates and commits no more than one persistence boundary:

1. The first turn accepts a `Scheduled` checkpoint and returns no permit.
2. A later turn prepares an attempt, accepts a separate `DispatchExposed`
   checkpoint revision, and only then returns an opaque `DispatchPermit`.
3. A completed checkpoint replays the recorded result.

The permit is evidence that this in-process host observed acceptance of both
persistence boundaries. It is **not** proof of exactly-once execution: a crash
may occur after exposure persistence and before, during, or after the external
effect.

## Quarantine and observation recovery

Reloading an unresolved `DispatchExposed` activity returns `Quarantined` with
the persisted logical and attempt identities before workflow-definition
matching. Quarantine does not redispatch, compensate, mutate the checkpoint,
or schedule a later activity. After an authoritative observation resolves the
exposed activity, ordinary replay resumes and still reports any definition
mismatch as nondeterminism before a later dispatch.

The host leaves effect execution to its caller. A caller may later supply an
authoritative `ActivityObservation` for the exact exposed logical activity.
An accepted observation records the exact result, after which normal replay
continues. Missing, stale, or mismatched observations are rejected; competing
observations use the same CAS rule. The crate does not determine the
observation's trust source or transport.

## Validation

Run the bounded feasibility evidence and the complete crate/workspace gates:

```console
cargo test -p kuberic-durable-execution --test feasibility -- --nocapture
cargo test -p kuberic-durable-execution --all-targets
cargo test -p kuberic-durable-execution --doc
cargo fmt --all -- --check
cargo check --workspace
cargo clippy -p kuberic-durable-execution --all-targets -- -D warnings
```

The feasibility test reruns the sole 20-scenario FR-013 registry, emits every
assertion, measures the FR-012 surface, and applies the exhaustive FR-014
three-way classifier.

## Limitations and exclusions

The result is limited to an in-memory synthetic model. The crate supplies no
production persistence, distributed execution ownership, worker, queue,
lease, activity handler, automatic observation polling, or passive-observation
transport. It does not establish a canonical exact-byte representation across
versions.

The bounded classifier reports `feasible`: all 20 registered FR-013 scenarios
and 54 structured assertions pass, all five FR-012 predicates pass, and no
in-scope limitation remains. The lost-reply fixture invokes one synthetic
effect under an opaque permit and discards its returned result before restart;
separate cases retain schedule, exposure, and observation CAS response-loss
coverage.

Timers, parallel activities, retries or backoff policy, cancellation, child
workflows, external events, compensation, migrations, upgrade guarantees,
rollout, or diagnostics are excluded. The experiment changes no Kuberic CRD,
operator reconciliation, durable topology workflow, status persistence,
ReplicaAgent, gRPC protocol, or deployment manifest. Its classification is
not a production-readiness, current-operator-parity, or adoption claim.
