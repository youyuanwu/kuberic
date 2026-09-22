# kuberic-protocol

Pure domain model for the level-triggered Kuberic stack.

The crate defines:

- replica identity, epoch, PC/CC, topology, provisioning, and transition types;
- normalized Kubernetes and replica-agent observations;
- fenced protocol commands and reconciliation plans;
- validation for quorum, incarnation, epoch, and transition invariants;
- the deterministic, side-effect-free evaluator.

It intentionally has no Kubernetes, gRPC, async runtime, or filesystem
dependencies. Controllers and agents exchange these canonical types through
transport adapters such as `kuberic-wire`.

The current evaluator covers Phase 1 behavior: initialization authority,
Kubernetes scaffolding, bootstrap intent, fresh-store initialization, stable
evidence, unsupported replica-count changes, waiting, and unsafe states.
