# Specification Research Questions: Framework-Native Switchover Graduation

**Target Branch**: `feature/framework-native-switchover`
**Issue URL**: none

## Intake Notes

The requested production outcome is one framework-native durable switchover
path with no public execution-mode selector or durable-pilot build feature.
The migration must preserve or strengthen all existing safety properties,
reuse the shared bounded in-process runner and ConfigMap checkpoint provider,
define a switchover-specific versioned contract and bounds, fail closed for
unsupported historical state, and leave add-replica, failover, initial
creation, SQLite issue #42, and unrelated protocol redesign out of scope.

The research must treat
`.paw/work/framework-native-remove-replica/` as read-only precedent and must
cite current code and documentation with exact file and line references.

## Internal System Behavior Questions

1. What are all current public and internal selectors for explicit versus
   durable-pilot switchover, including CRD fields, CLI or environment inputs,
   Cargo features, deployment manifests, examples, generated CRDs, CI
   commands, and documentation?
2. How does the explicit switchover state machine currently establish and
   validate admission identity, epochs, replica UID and generation fences,
   exact command correlation, write revocation, frozen-LSN capture, catch-up,
   demotion, promotion, epoch distribution, configuration publication,
   compensation, terminal completion, and restart recovery?
3. How does the durable-pilot switchover adapter currently map the explicit
   state machine into the durable execution framework, and which behavior,
   status, resource, measurement, or test surfaces are duplicated between the
   two paths?
4. What exact responsibilities are already centralized in the shared bounded
   in-process durable runner and ConfigMap checkpoint provider after
   framework-native remove-replica, and which operation-specific adapter
   responsibilities must switchover retain?
5. Which persisted legacy switchover status and checkpoint shapes can exist,
   how are they recognized today, and what conservative fail-closed behavior
   is required so an incompatible execution is never silently resumed,
   cleared, or replaced?
6. Does the current ReplicaAgent protocol provide enough primitives to
   preserve the intended production safety architecture, or does switchover
   require one coarse, versioned, correlated agent-owned intent? Identify the
   concrete safety and recovery evidence for either retaining operator-owned
   mutations or introducing the coarse intent.
7. If an agent-owned switchover intent is required, what existing add-replica
   and remove-replica contracts, admission rules, generation fencing,
   terminal replay, bounded status, and compensation patterns can be reused
   without broadening into unrelated ReplicaAgent or gRPC redesign?
8. What is the current canonical no-fault switchover path in terms of external
   effects, passive observations, completed durable boundaries, accepted
   writes, active checkpoint bytes, terminal checkpoint bytes, and terminal
   payload bytes?
9. What maximum-fault or worst-case switchover histories must be modeled to
   derive independent activity-count, input, result, active-checkpoint,
   terminal-checkpoint, terminal-payload, error-text, and reconciliation-fuel
   bounds rather than copying remove-replica limits?
10. Which tests currently prove switchover success, compensation, restart
    recovery, unknown-effect quarantine, exact command identity, UID and
    generation validation, topology publication, and terminal reload, and
    where are replacement gaps before legacy tests can be removed?
11. Which build, test, lint, formatting, generated-artifact, real-API, and
    measurement commands are authoritative for this repository, and which CI
    gates must change when the durable-pilot feature and selector are removed?
12. Which documentation and generated artifacts describe the old split or
    must record the graduated architecture, independent execution contract,
    measurements, compatibility behavior, and unchanged scope of other
    operations?
