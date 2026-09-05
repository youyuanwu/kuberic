# Kuberic Durable Execution Kernel

`kuberic-durable-execution` is an isolated feasibility crate for deterministic,
linear workflow replay. It has no dependency on `kuberic-core` or
`kuberic-operator`, and neither production crate depends on it. It is a kernel
for evaluating replay and provider contracts, not an end-user runtime.

## Selected authoring surface

The feasibility evaluation selected the ordinary async surface. A workflow is
an `#[async_trait(?Send)]` implementation of `Workflow::run`; its only
framework-specific workflow-body operation is
`WorkflowContext::activity(spec).await`. An immutable `ActivitySpec` combines
the versioned name, exact input, and declared maximum result bytes.

```rust
use async_trait::async_trait;
use kuberic_durable_execution::{
    ActivityName, ActivitySpec, ExactBytes, Workflow, WorkflowContext,
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
            .activity(ActivitySpec::new(
                ActivityName::new("greeting", 1).unwrap(),
                input,
                4096,
            ))
            .await
    }
}
```

The measured workflow body uses one framework operation against an FR-012
maximum of two. The public host reports ten turn/observation outcome variants,
including portable store failure. No explicit poll/replay authoring surface is
also exported.

## Replay and checkpoint semantics

`ExactBytes` are compared without normalization and encoded as validated base64
JSON strings. Workflow history is a contiguous, zero-based sequence with a
completed prefix and at most one final pending activity. A requested activity
must match the recorded sequence, immutable positive-versioned name, exact
input, and declared result bound; a mismatch is nondeterminism rather than a
new dispatch.

Format version 2 stores JSON payload bytes in a versioned
`CheckpointEnvelope`. The payload contains execution identity, exact workflow
input, declared activity specifications, and linear records in `Scheduled`,
`DispatchExposed`, or `Completed` state. Envelope version, payload shape,
activity count, encoded size, and completed-result bounds are validated before
the workflow is polled. Unsupported versions are compatibility errors, not
storage conflicts. Completed results replay into workflow code and do not
create a new dispatch permit.

Logical activity identity is the complete tuple of execution ID, sequence,
versioned activity name, exact activity input, and declared maximum result
bytes. Its external rendering contains the complete tuple without hashing or
normalization. `AttemptId` is a separate host-epoch and monotonic-counter
identity, so a discarded pre-exposure attempt may change without changing the
logical activity.

## Checkpoint limits and result reservation

Every `DurableHost` requires `CheckpointLimits` for maximum activity records
and maximum canonical encoded checkpoint bytes. Exact configured boundaries
are accepted; loaded or proposed checkpoints beyond either boundary are
rejected before workflow progress.

Before committing dispatch exposure, the host projects the completed
checkpoint containing a result at exactly the activity's declared maximum.
The projection calculates base64 and JSON lengths without allocating that
maximum result. If it cannot fit, exposure is rejected and no permit exists.
An observation larger than the declaration is rejected without mutation. This
ensures that a permitted activity result within its declaration cannot later
fail solely because its completed checkpoint lacks capacity.

## Storage revisions

`CheckpointStore` exposes runtime-neutral asynchronous `load` and
`compare_and_swap` operations. `load` distinguishes an absent checkpoint from
a portable classified `StoreError`; errors preserve a provider description
without exposing provider-specific types. `StorageRevision` is a validated
nonempty opaque string token with equality semantics and is unrelated to
checkpoint format version. It exposes no numeric, ordering, or increment API.

CAS returns only `Accepted`, `Conflict`, or `OutcomeUnknown`. A provider must
classify any result whose acceptance cannot be proven as `OutcomeUnknown`;
`StoreError` is reserved for definitely local or pre-request failures. A
conflict or unknown outcome returns `ReloadRequired`, while a provider error
returns `StoreFailed`. None grants dispatch permission. The included
`InMemoryCheckpointStore` keeps its numeric revision counter private, renders
opaque tokens, and can return the same unknown result with or without applying
the mutation.

## Turns and dispatch permission

Awaiting `DurableHost::turn` evaluates and commits no more than one persistence
boundary:

1. The first turn accepts a `Scheduled` checkpoint and returns no permit.
2. A later turn prepares an attempt, accepts a separate `DispatchExposed`
   checkpoint revision, and only then returns an opaque `DispatchPermit`.
3. A completed checkpoint replays the recorded result.

Construction requires the checkpoint store, caller-supplied host epoch, and
validated `CheckpointLimits`; callers supply the executor used to await turns
and observations.

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
CARGO_BUILD_JOBS=2 cargo test -p kuberic-durable-execution --test feasibility -- --nocapture
CARGO_BUILD_JOBS=2 cargo test -p kuberic-durable-execution --all-targets
CARGO_BUILD_JOBS=2 cargo test -p kuberic-durable-execution --doc
cargo fmt --all -- --check
CARGO_BUILD_JOBS=2 cargo check --workspace
CARGO_BUILD_JOBS=2 cargo clippy -p kuberic-durable-execution --all-targets -- -D warnings
```

The feasibility test reruns the sole conformance registry, emits every
assertion, measures the FR-012 surface, and applies the exhaustive FR-014
three-way classifier. The revised evidence contains 28 unique contiguous
scenarios and 81 structured assertions; all pass. All five FR-012 authoring
predicates and all four provider/bounding/documentation predicates also pass,
so the mechanically derived result remains **feasible** within this kernel's
stated boundary.

## Deferred usability roadmap

The current crate intentionally stops at the durable-execution kernel. The
following capabilities are ordered follow-up possibilities, not commitments,
and none is implemented here:

1. Typed serde activity and workflow adapters, plus durable activity failure
   results.
2. An activity registry/dispatch adapter and a passive convergence resolver.
3. Replay-aware tracing and checkpoint inspection.
4. Durable timers and retry policy only when a concrete Kuberic workflow
   demonstrates the need.
5. Instance lifecycle and query APIs, external events, parallel/join/select,
   child workflows, and worker/queue/lease facilities only on demonstrated
   need.
6. A Kubernetes checkpoint provider and one operator-workflow pilot only
   after the kernel/provider contract is proven.

This ordering is informed by the broader orchestration and provider surfaces
documented by
[Azure Durable Task Framework](https://github.com/Azure/durabletask) and
[microsoft/duroxide](https://github.com/microsoft/duroxide). It does not claim
API compatibility with either project.

## Limitations and exclusions

The result is limited to an in-memory synthetic model. The crate supplies no
production persistence, distributed execution ownership, worker, queue,
lease, activity handler, automatic observation polling, or passive-observation
transport. It does not establish a canonical exact-byte representation across
versions.

The classifier is recomputed from the current registry rather than assuming a
positive result. Provider cases distinguish absence from every portable error
class, exercise opaque revisions, and make applied and unapplied unknown CAS
outcomes indistinguishable to the host. The lost-effect-reply fixture still
invokes one synthetic effect under an opaque permit and discards its returned
result before restart.

Typed adapters, durable activity failures, dispatch registries, passive
convergence, tracing/inspection, timers, retries, parallelism, lifecycle,
queries, external events, child workflows, workers, queues, leases, and
Kubernetes/operator integration are excluded. So are compensation, migrations,
upgrade guarantees, rollout, and diagnostics. The experiment changes no
Kuberic CRD, operator reconciliation, durable topology workflow, status
persistence, ReplicaAgent, gRPC protocol, or deployment manifest. Its
classification is not a production-readiness, current-operator-parity, or
adoption claim.
