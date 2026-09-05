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
    ActivityName, ActivitySpec, ExactBytes, TerminalOutcome, Workflow,
    WorkflowContext,
};

struct Greeting;

#[async_trait(?Send)]
impl Workflow for Greeting {
    async fn run(
        &self,
        context: &mut WorkflowContext<'_>,
        input: ExactBytes,
    ) -> TerminalOutcome {
        TerminalOutcome::succeeded(
            context.activity(ActivitySpec::new(
                ActivityName::new("greeting", 1).unwrap(),
                input,
                4096,
            ))
            .await,
        )
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

Format version 3 stores JSON payload bytes in a versioned
`CheckpointEnvelope`. An immutable `ExecutionSpec` declares execution
identity, exact workflow input, and the maximum exact-byte terminal payload.
The persisted execution contract also records the encoded-checkpoint capacity
under which terminal state was admitted. Every load validates that authority
and rejects a smaller current limit before workflow polling.

The payload has exactly one explicit lifecycle state:

- `Active` contains the complete bounded linear history, with records in
  `Scheduled`, `DispatchExposed`, or `Completed` state.
- `Terminal` contains `Succeeded` or `Failed` exact bytes and the completed
  activity count. It has no activity history or digest.

Unsupported and prior formats are compatibility errors, not storage
conflicts. While active, completed activity results replay into workflow code
without a dispatch permit. Once terminal, a turn returns the stored outcome
and observed revision directly without polling workflow code.

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

Before an absent execution is evaluated, the host projects both success and
failure terminal checkpoints at the execution's declared maximum payload and
a maximum-width completed activity count. The projection uses checked
base64/JSON length arithmetic without allocating the declared payload. The
larger form must fit the configured encoded limit, which is then persisted as
the execution's immutable admission authority. Capacity failure therefore
precedes even the first schedule and every possible external-effect permit.
Later hosts may use an equal or larger limit, but not a smaller one.

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

## Kubernetes checkpoint provider spike

The default-off `kubernetes` feature adds `KubernetesCheckpointStore`. A caller
supplies a `kube::Client` and a namespace, retaining ownership of credential
discovery, the async runtime, namespace lifecycle, retries, and all workflow
effects:

```rust,no_run
# async fn example() -> Result<(), Box<dyn std::error::Error>> {
use kuberic_durable_execution::KubernetesCheckpointStore;

let client = kube::Client::try_default().await?;
let store = KubernetesCheckpointStore::new(client, "durable-checkpoints")?;
# let _ = store;
# Ok(())
# }
```

`KubernetesCheckpointStore::new` uses independently retained checkpoints and a
786,432-byte ConfigMap data budget. Advanced callers can use
`KubernetesCheckpointStore::with_options` with
`KubernetesCheckpointStoreOptions` to select a budget from 1 through 983,040
bytes and optionally attach one `KubernetesCheckpointOwner`. The owner wraps a
Kubernetes `OwnerReference` plus either a namespaced or cluster-scoped
assertion. A namespaced owner must name the checkpoint namespace. Required
identity fields and the immutable UID must be nonempty, and controlling or
deletion-blocking references are rejected before any request. The provider
does not fetch the owner, so the caller remains responsible for the accuracy
of its scope, GVK, name, and UID. Before a replacement, the provider reads the
current object and rejects a stale revision or owner relationship that differs
from the store configuration, so reconstruction cannot silently change a
checkpoint between independent and owner-based retention.

Each execution maps to
`kuberic-checkpoint-<32-lowercase-execution-id-hex>` in that namespace. The
format-3 envelope's canonical JSON is stored in
`data["checkpoint.json"]`; the object is labeled
`kuberic.io/component=durable-checkpoint`. Initial CAS uses create-if-absent.
Successor CAS uses replace with the caller's exact, opaque
`metadata.resourceVersion`. Already-existing creates, stale replacements, and
replacements racing with deletion return the portable unit `Conflict`; the
host's next load obtains authoritative state.

Explicit API rejections map to portable `StoreError` kinds and diagnostics
retain only the operation plus API code/reason. Mutation transport failures,
5xx responses, and successful responses without a usable revision are
`OutcomeUnknown`, because acceptance cannot be disproved. The provider never
embeds Kubernetes error types, credentials, checkpoint contents, or arbitrary
transport text in its public errors.

The data budget is the aggregate UTF-8 byte length of every ConfigMap `data`
key and value. An exact-bound object passes this local gate; one byte over is
rejected before dispatch. This provider check is separate from and does not
weaken the kernel's activity-count and encoded-checkpoint limits. Passing the
budget does not guarantee API-server acceptance: metadata, managed fields,
annotations, owner references, admission mutation, and API-server policy can
consume capacity or reject the object independently.

Checkpoints have no provider-driven terminal deletion. Terminalization
CAS-replaces active history with a terminal record and retains the ConfigMap.
Without an owner reference, the checkpoint remains independently retained
until an explicitly authorized lifecycle actor removes it. With an owner,
ordinary Kubernetes garbage collection may remove the checkpoint after that
owner is deleted. Orphan selection, retention policy, and recovery-safety
checks remain responsibilities of a separate lifecycle actor, not this
provider.

The namespace-scoped
[writer RBAC example](deploy/checkpoint-writer-rbac.yaml) grants only ConfigMap
`get`, `create`, and `update`. The separate
[cleanup RBAC example](deploy/checkpoint-cleanup-rbac.yaml) grants only
ConfigMap `list` and `delete`; it is optional and intentionally not granted to
the writer identity. These examples do not alter or broaden the existing
operator permissions.

`store.metrics().snapshot()` reports only writes confirmed by a response with a
usable authoritative revision. It includes accepted-write count, canonical
checkpoint JSON bytes, canonical typed server-returned ConfigMap JSON bytes,
and measurement failures. Object bytes include server metadata and are not raw
HTTP-wire bytes. The validation watch measurement similarly canonicalizes each
delivered typed `WatchEvent<ConfigMap>` and excludes HTTP framing and transport
overhead.

## Turns and dispatch permission

Awaiting `DurableHost::turn` evaluates and commits no more than one persistence
boundary:

1. For an activity-bearing workflow, the first turn accepts a `Scheduled`
   checkpoint and returns no permit.
2. A later turn prepares an attempt, accepts a separate `DispatchExposed`
   checkpoint revision, and only then returns an opaque `DispatchPermit`.
3. After observations make active replay complete, the host CAS-replaces the
   full active checkpoint with a minimal terminal checkpoint.
4. `WorkflowCompleted` is returned only after that CAS is accepted or a later
   load observes terminal state. It carries the exact success/failure outcome
   and accepted/observed opaque revision.

A zero-activity workflow skips schedule, exposure, and observation. Its first
turn validates terminal capacity and CAS-creates terminal state directly from
absence before returning `WorkflowCompleted`.

Construction requires the checkpoint store, caller-supplied host epoch, and
validated `CheckpointLimits`; callers supply the executor used to await turns
and observations.

The permit is evidence that this in-process host observed acceptance of both
persistence boundaries. It is **not** proof of exactly-once execution: a crash
may occur after exposure persistence and before, during, or after the external
effect.

Completion conflict or `OutcomeUnknown` returns `ReloadRequired` and never a
permit or completion. If an unknown write applied, reload observes terminal
state. If it did not apply, reload replays completed active results and retries
terminalization without redispatch. Provider failures return `StoreFailed`;
every later attempt starts with a fresh load.

Terminal success and failure payloads share the `ExecutionSpec` bound. An
exact-bound outcome is accepted. A larger outcome is an explicit checkpoint
contract violation and is not persisted or reported complete. This workflow
failure outcome does not add activity-failure behavior.

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

Enable the optional provider for deterministic client-surface validation:

```console
CARGO_BUILD_JOBS=2 cargo test -p kuberic-durable-execution --features kubernetes --test kubernetes_checkpoint -- --nocapture
CARGO_BUILD_JOBS=2 cargo test -p kuberic-durable-execution --features kubernetes --all-targets
CARGO_BUILD_JOBS=2 cargo clippy -p kuberic-durable-execution --features kubernetes --all-targets -- -D warnings
```

A feature-gated test performs authorization preflight, creates a temporary
namespace, validates the provider against the configured real Kubernetes API,
prints its measurement report, and waits for namespace deletion:

```console
CARGO_BUILD_JOBS=2 cargo test -p kuberic-durable-execution --features kubernetes --test kubernetes_checkpoint_real
```

The repository's existing [CI workflow](../.github/workflows/CI.yml) enables
all crate features on its workspace-wide test command after the existing
`helm/kind-action` step has provisioned the shared one-control-plane KinD
environment. There is no separate provider test step, second cluster, or
second cleanup owner. Cluster lifecycle remains owned by the existing KinD
action. Ordinary local and default Cargo test runs without the Kubernetes
feature do not select this test.

The test reports a failed endpoint or authorization precondition rather than
claiming real-API coverage. Its apply-then-unknown and no-apply-unknown cases
mask the store result around real persisted state; they validate host recovery,
not reproduction of an actual network fault.

The feasibility test reruns the sole conformance registry, emits every
assertion, measures the FR-012 surface, and applies the exhaustive FR-014
three-way classifier. The revised evidence contains 38 unique contiguous
scenarios and 117 structured assertions; all pass. All five FR-012 authoring
predicates and all five provider, bounding, lifecycle, and documentation
predicates also pass, so the mechanically derived result remains **feasible**
within this kernel's stated boundary.

## Deferred usability roadmap

The crate intentionally stops at the durable-execution kernel.
Completion-only compaction and an isolated Kubernetes checkpoint-provider spike
are implemented; generic active-history compaction, continuation, and operator
adoption remain excluded. The remaining ordered deferred work is tracked in
[Durable Execution Framework Roadmap](../docs/features/kuberic/durable-execution-roadmap.md).

## Limitations and exclusions

The kernel result remains experimental. Its opt-in ConfigMap provider and
real-API spike do not establish production persistence fitness, distributed
execution ownership, a worker, queue, lease, activity handler, automatic
observation polling, or passive-observation transport. It does not establish a
canonical exact-byte representation across versions.

The classifier is recomputed from the current registry rather than assuming a
positive result. Provider cases distinguish absence from every portable error
class, exercise opaque revisions, and make applied and unapplied unknown CAS
outcomes indistinguishable to the host. The lost-effect-reply fixture still
invokes one synthetic effect under an opaque permit and discards its returned
result before restart.

Typed adapters, durable activity failures, dispatch registries, passive
convergence, tracing/inspection, timers, retries, parallelism, generic
lifecycle APIs, queries, external events, child workflows, workers, queues,
leases, and operator integration are excluded. So are compensation,
migrations, upgrade guarantees, rollout, and production diagnostics. The
experiment changes no Kuberic CRD, operator reconciliation, durable topology
workflow, status persistence, ReplicaAgent, gRPC protocol, or deployment
manifest. These provider-readiness prerequisites do not establish
current-operator parity or authorize an operator pilot, switchover, or adoption.
