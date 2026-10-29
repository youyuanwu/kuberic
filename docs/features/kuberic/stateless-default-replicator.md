# Default Replicator: Process-Local Engine, Service-Owned State

“Stateless” here describes process-local engine queues/sessions, not absence of
durable application or agent state. The selected implementation is classic V1,
constructed by the sole built-in factory
([factory:1138–1156](../../../kuberic-runtime/src/replicator/mod.rs#L1138-L1156)).
Use [current semantics](classic-v1-runtime.md) for the authoritative behavior,
[ownership boundary](replicator-boundary.md) for integration responsibilities,
and [pending work](sf-v1-semantic-alignment-plan.md) for remaining alignment.
The [ordinary-engine hosting redesign](engine-neutral-hosting.md#proposed--ordinary-engine-boundary)
is **PROPOSED / NOT IMPLEMENTED**. Process-local queues do not imply removable
agent authority or PostgreSQL metadata; those have separate recovery owners.

## Ownership

| Owner | State / responsibility | Current evidence |
|---|---|---|
| Replica agent / host | Durable lifecycle authority and serving fences, not application commitment | [selected lifecycle:14–44](../../../kuberic-runtime/src/host/public_lifecycle_program.rs#L14-L44), [access:692–756](../../../kuberic-runtime/src/host/hosting.rs#L692-L756) |
| Built-in classic engine | Live queues/peers/ACK/copy/role state; process-local composition | [production:127–227](../../../kuberic-runtime/src/replicator/v1/service_adapter/production.rs#L127-L227) |
| Application service/provider | Durable preparation, local commit, visibility/publication and snapshots | [KV:195–240,324–376](../../../examples/kvstore2/src/v1.rs#L195-L376), [SQLite:272–345](../../../examples/sqlite/src/v1.rs#L272-L345) |

Early LSN assignment and quorum completion do not certify local commit or client
success ([production:473–491](../../../kuberic-runtime/src/replicator/v1/service_adapter/production.rs#L473-L491)).
An independent provider needs no durable-progress helper hook
([public journey:1861–1939](../../../kuberic-runtime/tests/classic_v1_production.rs#L1861-L1939)).
Native queues are not a recovered application write journal.

## Deployment and recovery constraints

The selected format constants are public protocol 13 / agent schema 14
([protocol:13](../../../kuberic-runtime/src/protocol/public_operations.rs#L13),
[schema:19–21](../../../kuberic-runtime/src/host/state.rs#L19-L21)).
This reference does not promise old-state migration, mixed-version enrollment
or transparent completion of crash-interrupted requests. Application retry and
ambiguity decisions must not treat a cancelled observer as rollback.

Transport uses agent bearer credentials over HTTP, **not credential-free
transport and not TLS**
([HTTP:177–227](../../../kuberic-runtime/src/replicator/v1/service_adapter/production_transport.rs#L177-L227),
[bearer:728–766](../../../kuberic-runtime/src/replicator/v1/service_adapter/production_transport.rs#L728-L766)).
Correctness identities/fences are independent of authentication. Protect network,
tokens and namespace resources separately; see
[security qualification](sf-v1-semantic-alignment-plan.md#deployment-security-qualification).

Direct Close, Abort containment requests, Remove and explicit provider/task
joins are not the same postcondition
([actual Close/join witness:351–387](../../../kuberic-runtime/src/replicator/v1/service_adapter/tests/facade.rs#L351-L387)).
Source/component/application evidence is not a production-readiness or native
differential claim. [Testing](testing.md) gives current reproduction tiers.
