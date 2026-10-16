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

The current test inventory contains 301 tests:
255 ordinary, 41 PostgreSQL, three KinD, and two build-only.

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
