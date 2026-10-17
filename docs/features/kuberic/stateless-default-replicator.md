# Stateless Default Replicator

## Status

Public V1 is the sole built-in Kuberic replication path. The legacy managed
engine, its private lifecycle/data-plane attachments, continuation stores,
agent-owned replication dispatcher, old protocol branches, and runtime
selection/fallback have been removed.

The implementation is experimental and validated for development and CI. It
does not include published images or an approved production-activation
procedure.

## Ownership Model

The replacement uses a Service Fabric V1-shaped ownership boundary:

| State or behavior | Owner |
|---|---|
| Replica identity, history, lifecycle intent, exact operation authority, and terminal completion | Local agent schema 12 |
| Application bytes, applied/committed progress, retained operations, and copy snapshots | `StateProvider` / `DurableState` |
| Replication queues, peer sessions, ACK aggregation, copy streams, and listener lifetime | Built-in stateless Replicator memory |
| Desired replica count, Kubernetes resources, accepted Public V1 status, and command dispatch | Namespace-bound controller |

The built-in engine is recreated after process restart from agent-owned
authority and provider-owned durable state. It has no Replicator-owned
continuation database.

## Construction and Hosting

Applications create the built-in implementation with
`DefaultReplicatorFactory`. `CreateReplicator` returns one coherent public
interface bundle containing the `Replicator`, optional `PrimaryReplicator`,
and optional `StateReplicator`.

The protected method sets of `Replicator`, `PrimaryReplicator`,
`StateReplicator`, `StateProvider`, and `DurableState` are unchanged.

Those method inventories describe the completed replacement, not proof of
native semantic parity. The [faithful Rust async/tonic port roadmap](sf-v1-semantic-alignment-plan.md)
replaces this algorithm using native V1 state machines, with corresponding
service/persistence ownership and unprotected public surface changes;
only `Replicator`, `PrimaryReplicator`, and `StateProvider` must retain their
signatures. The runtime behavior described on this page is unchanged by planning.

There is no engine selector or legacy fallback:

- Public V1 identity version 11 is the only accepted built-in protocol
  identity.
- `ReplicatorSettings` contains the replication address, not a protocol
  selection flag.
- one Open owns one replication endpoint and at most one provider writer;
- duplicate, crossed, stale, or mismatched construction fails closed.

## Fresh State and Rejection

The current agent schema is 12. A fresh store is created directly at schema
12, and an accepted schema-12 store can be reopened.

Schemas 6, 7, and 11, partial schema-12 stores, and supported Phase 6 provider
formats are rejected before writable setup. Rejection tests compare complete
file trees and bytes so classification cannot mutate the rejected state.
Independent custom Replicators own classification of their own provider
formats.

There is no migration API, migration ledger, rolling compatibility mode,
mixed-version enrollment, export/reimport procedure, or fallback reader.

## Replication and Recovery

The Replicator starts access-closed and validates agent authority against
provider progress before serving.

- writes acknowledge only after provider durability and required PC/CC quorum
  completion;
- incomparable histories are not ordered by numeric progress;
- stale sessions, storage incarnations, epochs, configurations, and build
  attempts are rejected;
- ambiguous provider application fences serving until the exact suffix is
  resolved or the incarnation is replaced;
- build copies a frozen provider snapshot and then catches up through the
  retained-operation boundary;
- retirement permanently excludes the old storage incarnation;
- Close and Abort cancel and drain owned descendant work;
- completed inbound and outbound transport-task entries are reclaimed during
  ordinary operation rather than retained until shutdown.

### Process-session renewal

The topology stores the accepted process session and replicator creation
identity for every member. A restarted process on the same storage incarnation
cannot replace those values directly.

If the runtime is not yet open, the controller permits only a contained
existing-state Open. It then persists one renewal authority containing the
predecessor, replacement endpoint, surviving sessions, next epoch, and fresh
configuration. Participants close access, durably advance survivor epoch
barriers, replay the unchanged role at the new epoch, install exact peer
descriptors, and record authorization before releasing local fences.

Participant journals may first observe a later accepted stage. They seed or
fast-forward only through validated adjacent transitions with identical
required authority. A later renewal may supersede an obsolete local journal
only when its captured predecessor topology is at least the earlier renewal
epoch.

### Replacement and failover

Same-cardinality replacement binds one new Pod/PVC storage incarnation,
installs exact inbound Build authority before copy, admits the replacement only
after Build completion, and retires the old storage incarnation. Overlapping
predecessor/replacement reports remain distinct; topology commands select only
the durably bound replacement.

Failover continues probing the unavailable primary during the configured
delay. After expiry, only that primary is omitted from observation. Surviving
secondaries receive configuration before the promoted primary; the promoted
candidate is stable through catch-up, and `CatchUp(All)` completes before
writer access is restored.

## Controller Integration

The controller observes one installation namespace and creates the support
Secret, peer Service, write Service, desired PVCs, Pods, and exact per-replica
endpoints before consuming agent reports. A preseeded report cannot bypass
resource convergence.

The CRD status is structurally typed and contains only Public V1 authority,
topology, session-renewal, replacement, transition, and operation state. Old
or partial controller status is rejected before planning or effects.

Controller watches and mutations are namespaced. Deployment uses
`KUBERIC_INSTALLATION_NAMESPACE`, and RBAC uses `Role`/`RoleBinding`; there is
no cutover Lease or cluster-wide mutation authority.

## Security Boundary

Public V1 replication transport is credential-free by design. Resource,
replica, process-session, epoch, history, storage, operation, and build values
are correctness fences, not credentials.

The controller command bearer token and namespace RBAC protect a separate
control-plane boundary. They do not add peer authorization to replication
transport.

## Verification

Routine verification uses existing repository paths:

```bash
cargo fmt --all -- --check
cargo check --workspace --all-targets --all-features
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --doc --workspace --all-features
scripts/check_runtime_public_api.sh
just nextest-test
just nextest-build-only
just nextest-postgres-smoke
just nextest-archive
just nextest-validate-archive
```

The current exact inventory is documented in [Testing](testing.md). Live KinD
validation uses only bootstrap, replacement, and failover in an explicitly
owned disposable cluster.

## Limitations

- no published release images or supported installation;
- no production activation or traffic-movement procedure;
- no old-state preservation, migration, or mixed-version operation;
- no Public V1 transport authentication or authorization;
- no transparent completion guarantee for a client request interrupted by
  process loss;
- no optional V2 transactional Replicator.

Any future activation, transport-credential, or V2 project is separately
scoped. It must not reintroduce a second built-in engine or compatibility
path.
