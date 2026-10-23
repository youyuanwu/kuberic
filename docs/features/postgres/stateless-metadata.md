# PostgreSQL Restart-Stateless Metadata — Superseded Proposal

**Historical research, not current behavior or an active implementation plan.**
The [original draft][historical] remains available at the immutable research
baseline. Its transfer of PG-specific policy/certificates/recovery evidence
into generic AgentStore/controller facilities was rejected. Its blanket
“custom replicator must persist no state” requirement is not adopted.

Current [PostgreSQL implementation](design.md) retains checksummed application
metadata plus PGDATA. The [ordinary-engine hosting redesign](../kuberic/engine-neutral-hosting.md#proposed--ordinary-engine-boundary)
is **PROPOSED / NOT IMPLEMENTED** and retains that metadata initially.
Removing a mandatory controller companion does not prove reconstructibility
of native policy, process ownership, interrupted effects or acknowledged data.

Use only:

- the [single future runtime roadmap](../kuberic/sf-v1-semantic-alignment-plan.md#hosting-redesign-roadmap--not-implemented)
  for generic controller/agent/host/engine migration;
- the [deferred metadata-disposition gate](stateless-metadata-prerequisites.md)
  for a complete per-field/read-write/crash/reconstruction proof before any
  separate metadata removal;
- [current semantics](../kuberic/classic-v1-runtime.md) and
  [testing/evidence limits](../kuberic/testing.md) for present contracts.

There is no new metadata schema, automatic upgrade, import or migration
implementation here. Historical concern labels do not establish current
defects, completed tests, native equivalence or downstream qualification.

[historical]: https://github.com/youyuanwu/kuberic/blob/b37009f8af7a7b2ed91f58f4889ed74e06013861/docs/features/postgres/stateless-metadata.md
