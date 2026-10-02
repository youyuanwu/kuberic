# Kuberic V1 Removal Record

> **Status: Complete.** The classic v1 runtime, operator, KVStore application,
> integration tests, deployment assets and image publication have been removed.
> This document records the completed migration/removal decision; it is not an
> operational guide.

## Decision

Kuberic v1 was removed before v2 distribution because there were no active
users or deployments requiring a compatibility period. The repository did not
need a deprecation release, user notification, resource conversion, application
data migration, fallback implementation or coexistence mode.

The previous sequence—publish v2, deprecate/freeze v1, then remove v1—was
superseded by this explicit no-active-users clean-removal decision. Existing
external classic registry artifacts, if still present, are unsupported
historical artifacts and were not deleted by the source change.

V2 distribution remains a separate future workstream. The repository currently
publishes no Kuberic controller or application images and provides no supported
release installation path. Source builds and isolated local/CI deployment
assets remain available.

## Removed Surface

The removal deleted:

- the classic replication/runtime package;
- the classic Kubernetes operator and its CRDs/RBAC;
- the classic KVStore application and generated API;
- classic Gateway, node-maintenance and lease-election integration tests;
- classic Dockerfiles, manifests, Gateway routes and external chart tooling;
- classic main/tag image publication;
- operational classic API, deployment and maintenance documentation.

Historical architecture and ADR material remains archived where it explains
past decisions.

## Completed V2 Workstreams

### Workstream 1: Planned Switchover

V2 supports explicit named-target planned switchover with durable handoff,
write closure, restoration before newer authority, and fail-closed terminal
outcomes.

### Workstream 2: Scale Up and Scale Down

V2 supports sequential one-at-a-time scale-up and secondary-only scale-down.
Move primary authority with planned switchover before reducing membership.

### Workstream 3: SQLite on V2

The existing SQLite application was migrated in place. It uses
quorum-before-publication WAL-frame replication, durable committed snapshots,
retained catch-up and restart/rebuild fencing. Fresh v2 storage is required.

### Workstream 4: PostgreSQL on V2

The existing PostgreSQL application was migrated in place as an SF-style
custom replicator. PostgreSQL owns WAL, physical replication/recovery,
synchronous policy, replay and promotion; Kuberic owns generic authority and
lifecycle choreography. Fresh protocol-9/schema-5 storage is required.

### KVStore2 Conformance Application

KVStore2 remains the isolated live conformance application for the
level-triggered controller, agent and runtime.

## Current Validation Contract

The surviving repository validates:

1. protocol, wire, runtime, agent and controller unit/model/durable behavior;
2. SQLite unit and in-process lifecycle/reconfiguration scenarios;
3. PostgreSQL host-local process, recovery, fencing and scaling scenarios;
4. SQL Server server-free observation behavior;
5. DEX default, mocked-provider and isolated real-Kubernetes behavior;
6. KVStore2 bootstrap, replacement, failover, quorum-loss, switchover,
   scale-down, scale-up and adversarial KinD scenarios.

No current validation requires the removed classic stack.

## Distribution Status

V2 controller/KVStore2 Dockerfiles, manifests and Just recipes are retained as
experimental local/CI assets. SQLite and PostgreSQL do not have supported
application images or deployment manifests. Publishing immutable images,
versioned installation assets and compatibility guidance remains deferred.

## Deferred Scale-Down Follow-Ups

The following remain outside v1 removal:

- automatic or atomic direct-primary removal;
- independent target/minimum replica policy;
- configurable PVC retention and storage size;
- validated maximum replica-count guidance;
- automatic rolling image/protocol upgrades;
- node-maintenance orchestration for v2;
- application data import or resource conversion.

These items are v2 roadmap work, not prerequisites for the completed classic
source deletion.

## Preserved Independent Components

SQL Server observation and DEX never depended on the classic implementation and
remain supported source components. Independent protobuf namespaces containing
`v1`, Service Fabric V1 terminology, DEX `kuberic.io` metadata and local
`level-triggered-v1` development image tags do not represent classic Kuberic
compatibility.
