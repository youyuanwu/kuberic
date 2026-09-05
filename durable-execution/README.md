# Kuberic Durable Execution Experiment

`kuberic-durable-execution` is an isolated feasibility crate for deterministic,
linear workflow replay. It is not used by the Kuberic operator or
`kuberic-core`.

Phase 1 evaluates a provisional ordinary-async authoring surface:

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

The only workflow-body operation is `WorkflowContext::activity`. Evaluation
polls one deterministic turn: completed records return their exact result, a
new activity produces a versioned checkpoint containing one scheduled record,
and changed history produces a nondeterminism result. Checkpoints are JSON
payloads inside an envelope with an explicit format version. Validation occurs
before workflow code is polled.

Logical activity identity is the complete tuple of execution ID, zero-based
sequence, caller-supplied name and positive version, and exact input bytes.
Its stable external rendering includes that entire tuple directly. It does not
hash or normalize any tuple member.

Run the focused Phase 1 checks with:

```console
cargo check -p kuberic-durable-execution --all-targets
cargo test -p kuberic-durable-execution --test replay --test authoring_candidate
cargo clippy -p kuberic-durable-execution --all-targets -- -D warnings
cargo fmt -p kuberic-durable-execution -- --check
```

This phase deliberately has no storage host, compare-and-swap implementation,
dispatch permit, activity handler, worker, observation transport, or effect
invocation. The overall experiment also excludes timers, parallel activities,
cancellation, child workflows, external events, queues, leases, retries,
production persistence, migration, compatibility guarantees, operator/CRD or
gRPC changes, deployment changes, production-readiness claims, and any claim
that Kuberic will adopt this framework.
