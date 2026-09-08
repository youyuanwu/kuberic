# Specification Research Questions: Framework-Native Remove Replica

**Target branch**: `feature/framework-native-remove-replica`
**Issue URL**: none

## Agent Notes

This work is a committed production migration, not another pilot evaluation. Remove-replica must become the default and only operator orchestration path. The implementation must preserve the existing safety inventory, replace pilot-specific dual-path surfaces, materially reduce avoidable checkpoint payload repetition, and establish a reusable production runner suitable for a later add-replica migration. Breaking API changes are allowed, but old checkpoints must never be silently misinterpreted. `ReplicaAgent` and gRPC behavior remain out of scope unless research reveals a protocol defect that requires a material user decision.

## Internal System Behavior Questions

1. Where are the explicit and durable remove-replica orchestration paths selected, hosted, recovered, and reconciled today, and which configuration, CRD, telemetry, and deployment surfaces exist solely for dual-path pilot operation?
2. What durable runner machinery is duplicated between switchover and remove-replica for checkpoint interpretation, fused progression, CAS/unknown-outcome recovery, quarantine, permit handling, terminal handoff, and bounded requeue behavior?
3. What is the current durable remove-replica workflow state/activity contract, which fields repeat full state or configuration data, and what authoritative safety proofs consume those fields?
4. What explicit remove-replica safety and regression tests exist, and where is each required invariant from the user-provided safety inventory currently asserted?
5. What checkpoint format/version/admission behavior exists for switchover and durable remove-replica, and how are invalid, corrupt, incompatible, active, and terminal checkpoints distinguished?
6. What does the authoritative checkpoint measurement fixture report for merged PR #59, including accepted writes, active-checkpoint maxima, terminal-checkpoint size, and terminal-payload size?
7. Which documentation, examples, and automation still frame remove-replica as a pilot, expose explicit/durable selection, or retain obsolete pilot-era reporting?
8. What repository conventions govern commit/PR titles, validation commands, live Kind tests, image loading, and Kubernetes owner/retention behavior?
9. Which existing shared abstractions and extension points can support a maintainable operator durable runner without speculative queue/worker generalization, and what add-replica-specific needs should the interface leave open?
10. Does the current `ReplicaAgent` or gRPC contract prevent a compact framework-native remove-replica workflow from preserving exact command matching, fencing, commit authority, cleanup proof, or conservative unknown-outcome recovery?
