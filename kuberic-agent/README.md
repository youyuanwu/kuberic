# kuberic-agent

Replica-local hosting and durable authority for the level-triggered Kuberic
stack.

`kuberic-agent` owns the Service Fabric Replica Agent/FUP-equivalent process
boundary:

- application lifetime, `Open` registration, role/close/abort ordering, and
  exact returned-replicator identity;
- one SQLite metadata database under `.kuberic/agent.sqlite3` on the replica
  PVC, separate from application data;
- exact resource, Pod, PVC, replica-incarnation, durable-generation,
  initialization, policy, and schema identity;
- intent-before-effect and terminal-result-before-reply ordering;
- restart recovery from pending intent or retained terminal evidence;
- ephemeral process sessions and session-scoped report sequences.

The database is created only by an authorized `InitializeAgentStore` path.
Missing established metadata, corruption, incompatible schema, or identity
mismatch fails closed instead of creating empty authority. SQLite uses WAL and
`synchronous=FULL`; the agent is the single writer.

Primary promotion follows the SF host sequence: replicator role, epoch and
state-provider update, then application role. Abort stops the returned control
before application teardown. The default replicator's managed capability is
transferred directly into agent registration and is not returned to
application code.

The crate currently exposes runtime-domain forwarding for the future transport.
Network listeners, reliable peer sessions, declarative reconfiguration
coordination, durable recovery of individual promotion substages, partition
read/load/fault reporting, and protobuf conversion remain Phase 4 work.
