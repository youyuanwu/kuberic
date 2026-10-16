# Service Fabric Alignment and Runtime Simplification

## Status

The Public V1 replacement is implemented. Kuberic now uses a single
Service Fabric V1-shaped built-in Replicator boundary rather than a managed
engine plus a dormant replacement.

This document records the resulting architecture and remaining deliberate
differences. It is not a cutover or migration runbook.

## Aligned Ownership

| Concern | Kuberic owner | Service Fabric analogue |
|---|---|---|
| Replica identity, epoch, PC/CC authority, lifecycle intent | Local agent | Reconfiguration Agent |
| Desired resources and accepted cluster status | Controller | Failover Manager |
| Application state, progress, retained history, copy representation | State provider | Stateful service/state provider |
| Live transport, queues, peers, ACKs, copy and quorum mechanics | Stateless built-in Replicator | V1 Fabric Replicator |

The V1 Replicator is restart-reconstructable. Application durability remains
provider-owned, and topology/lifecycle durability remains agent-owned.

## Completed Simplifications

- one built-in factory and one protocol identity;
- no managed lifecycle or data-plane capability attachment;
- no engine selection or fallback;
- no legacy continuation tables or migration ledger;
- no agent-owned replication listener or peer dispatcher;
- Public V1 command, report, history, session, operation, and evidence paths
  are ordinary production contracts;
- controller mutation authority is confined to one namespace;
- completed transport tasks are reclaimed during normal operation;
- old agent, controller, and supported provider state rejects unchanged.
- fresh agent initialization, exact process-session renewal, copy-based
  replacement, delayed failover, catch-up, and old-primary fencing are
  ordinary level-triggered production paths.

The public `Replicator`, `PrimaryReplicator`, `StateReplicator`,
`StateProvider`, and `DurableState` method sets remain stable.

## Stateful API and Trait Compatibility

Kuberic is SF V1-shaped, not a drop-in implementation of the SF APIs.
`Replicator` and `PrimaryReplicator` expose nearly the same lifecycle and
reconfiguration operations. The larger differences are at the service,
replication-completion, and persistence boundaries.

The comparison below uses the Rust interfaces in
[service-fabric-rs stateful traits](https://github.com/Azure/service-fabric-rs/blob/main/crates/libs/core/src/runtime/stateful_traits.rs)
and [mssf-ext traits](https://github.com/youyuanwu/service-fabric-apps-rs/blob/main/crates/libs/mssf-ext/src/traits.rs).
It concerns V1 interfaces, not SF's V2 transactional state-provider contract.

| Surface | Referenced SF interfaces | Kuberic |
|---|---|---|
| Replicator control | Open, role/epoch changes, progress, PC/CC configuration, catch-up, build, removal, data loss, Close and Abort | Nearly the same operation sets. Some synchronous SF queries/configuration calls are async here. Kuberic also explicitly replays primary epoch barriers rather than assuming `update_epoch` is secondary-only. |
| Replication completion | `replicate` exposes the assigned LSN immediately and a receiver for eventual completion | `replicate(...).await` returns the LSN only after completion. There is no early-LSN result with which a service can prepare its local commit while replication is pending. |
| Persistence integration | A service implements `StateProvider` and applies/acknowledges operations received from copy and replication streams | The built-in engine additionally requires `DurableState`: apply, commit, progress, retained-history, copy and verification hooks. This is an additional integration boundary, not a method extension to `StateProvider`. Custom replicators may own their own persistence instead. |
| Cancellation | Many operations take an explicit cancellation token | Public trait methods omit per-call cancellation tokens. Dropping a future is not an equivalent cancellation protocol or proof of rollback. |
| Service construction | The linked service Open returns a primary-replicator interface | Open takes `OpenContext` through `Arc<Self>` and returns `Arc<dyn Replicator>`. Creation separately registers a coherent `ReplicatorInterfaces` bundle with optional state/primary interfaces. |
| Operation acknowledgement | `acknowledge()` reports successful application; volatile SF services can use implicit acknowledgement | `StreamOperation::acknowledge` consumes the operation and supplies applied/committed progress. Every successfully applied stream operation requires explicit acknowledgement; application failure can be reported with `reject`. |
| Replica/configuration input | SF identity, role, address, progress and quorum descriptions | Kuberic also exposes build IDs, process sessions, storage incarnations and exact configuration identifiers. These are required authority inputs, not interchangeable numeric progress hints. |
| Runtime API coverage | Partition health, replica health, move cost, credential settings, settings retrieval and COM interop are present in the linked interfaces | Kuberic uses a concrete partition handle with a smaller surface. Settings currently expose only the replication address, without credentials or a settings getter; several SF operations have no counterpart. |
| Data and stream representation | Separate operation-data/stream traits and buffer-access abstractions | `OperationData` is `Bytes`; provider data streams are boxed Rust streams. Public interfaces use `Arc` trait objects and structured operation metadata. These are primarily Rust representation differences. |

Application-facing trait definitions are in
[`traits.rs`](../../../kuberic-runtime/src/traits.rs), and replication trait
definitions are in
[`replicator/traits.rs`](../../../kuberic-runtime/src/replicator/traits.rs).
Re-exports preserve the existing crate-root, `application`, and `replicator`
import paths. Data types and interface assembly remain in
[`application.rs`](../../../kuberic-runtime/src/application.rs) and
[`replicator/mod.rs`](../../../kuberic-runtime/src/replicator/mod.rs);
operation streams and persistence hooks are in
[`replicator/stream.rs`](../../../kuberic-runtime/src/replicator/stream.rs) and
[`engine.rs`](../../../kuberic-runtime/src/engine.rs). See
[Replicator Boundary](replicator-boundary.md) for ownership and creation rules.

Porting an SF service therefore requires more than renaming methods. In
particular, an early-LSN local-commit pipeline cannot be carried over unchanged,
and a service using the built-in engine must integrate the separate persistence
hooks and explicit durable progress. `DurableState` does not make the V1 engine
a V2 Logging Replicator or transfer topology durability out of the agent.

### Current Behavioral Gaps

API shape and intended architectural differences do not establish behavioral
compatibility. In particular:

- SF obtains copy context on an idle secondary and sends it to the primary.
  The built-in Kuberic engine currently obtains context from the source
  provider instead; it does not implement that bidirectional exchange.
- Copy/replication stream getters exist, but the built-in engine currently
  delivers incoming operations directly to `DurableState`, not through those
  service streams. This is the open SF-14 contract gap, not proof that the
  documented service-delivery interface works.
- The built-in catch-up capability reports a committed tail rather than the
  retained floor described by the contract. SF-D01 tracks the required
  documentation/behavior reconciliation.

The [Contract Audit Tracker](service-fabric-contract-audit.md) records these
numbered findings and the other open implementation violations. Documentation
alignment alone does not close them or establish full SF compliance. The
protected `Replicator`, `PrimaryReplicator`, and `StateProvider` interfaces
remain unchanged.

## KSF Conformance

The shared KSF catalog contains exactly KSF-01 through KSF-16. It combines
behavioral scenarios with one production-derived ownership inventory.

KSF-09, KSF-10, and KSF-16 inventory:

- built-in factories;
- host attachments;
- gRPC services/endpoints;
- report branches;
- SQLite tables;
- startup owners;
- protocol versions and admission/replay sources;
- controller modules and feature-gated test harness registrations.

The validator accepts one Public V1 protocol and one controller ownership
surface. Extra endpoint, alternate protocol, and alternate controller-owner
fixtures are injected through the same discovery path and must fail the same
validator.

The current test inventory contains 337 tests:
291 ordinary, 41 PostgreSQL, three KinD, and two build-only.

## Deliberate Differences

- Kubernetes resources replace Service Fabric placement objects.
- The controller is level-triggered and reconstructs work from observation
  rather than persisting an operator phase cursor.
- Process-session renewal is explicit durable authority: contained Open,
  survivor epoch barriers, replacement role replay, exact configuration, and
  authorization precede restored access.
- Kubernetes failover uses a configurable observation delay rather than a
  Service Fabric placement subsystem; recovered primaries clear the marker
  before expiry, while sustained outages promote and catch up a survivor.
- Public V1 transport has no credential contract.
- Application-specific providers define their own durable formats and
  old-format rejection.
- Kuberic couples desired replica target and minimum through
  `spec.replicas`; it does not claim complete Service Fabric policy parity.

## Optional Future V2

A V2 transactional layer would require an explicit application transaction
log, provider checkpoint, replay, and backup contract. The removed managed
continuation records are not a V2 implementation and must not be restored
under a new name.

V2 remains optional and separately scoped after the Public V1 replacement.

## Operational Boundary

The repository provides source builds, nextest archives, development
Dockerfiles, manifests, and owned KinD validation. It does not publish images
or define production activation.

There is no Kubernetes cutover Lease, compatibility adapter, old-state
migration, digest-bundle workflow, namespace quarantine, or export/reimport
procedure. Any future production rollout is a new project using fresh state.
