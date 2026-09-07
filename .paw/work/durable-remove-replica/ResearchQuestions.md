# Specification Research Questions: Durable Remove Replica

**Target Branch**: `feature/durable-remove-replica`
**Issue URL**: none

## Agent Notes

The work ports `remove_replica` as the second workflow hosted by the experimental
durable-execution kernel. The existing explicit remove-replica path must remain
the default, and the new path must be protected by an explicit default-off Cargo
feature. The work must preserve the kernel's safety, determinism, admission, and
terminal-ordering contracts while producing honest complexity and checkpoint
measurements. Documentation must also correct the stale feasibility predicate
and post-PR-48 roadmap claims.

All review and plan-generation activities use single-model `gpt-5.6-sol`.

## Internal System Behavior Questions

1. Where and how is the current explicit remove-replica workflow selected,
   initialized, advanced, persisted, resumed, and surfaced through status?
2. What are the complete externally visible commands, authoritative
   observations, state transitions, fencing checks, retry/unknown-outcome
   policies, quarantine behavior, and terminal-ordering guarantees of the
   explicit remove-replica workflow?
3. Which switchover pilot components are genuinely workflow-independent and can
   be reused unchanged, and which components currently encode switchover-only
   assumptions that must be generalized?
4. How do prepared effect exposure, typed activities, fused progression,
   checkpoint admission, terminal compaction, and the ConfigMap checkpoint store
   integrate in the existing switchover pilot?
5. What feature-gating and reconciler-test patterns establish that the
   switchover pilot is default-off and leaves explicit behavior unchanged?
6. What exact non-overlapping source scopes does the current complexity script
   charge to the switchover workflow body, comparable legacy scope, shared
   reusable infrastructure, and operator integration?
7. Which existing tests define the remove-replica lifecycle regression matrix
   and which pilot tests provide patterns for replay, effect, checkpoint, and
   reconciler coverage?
8. How is the feasibility classifier derived, and how can its runtime-neutrality
   predicate inspect only library dependencies while still rejecting a real
   runtime dependency?
9. Which README and roadmap statements are stale after PR #48, and what current
   evidence or contracts should replace them?
