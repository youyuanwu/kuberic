# Level-Triggered Kuberic Operator Design

## Status

Implemented as the experimental `operator.kuberic.io/v1alpha1` stack. The
as-built user and deployment contract is documented in
[Level-Triggered Kuberic Operator](../features/kuberic/level-triggered-operator.md).

This proposal records the design rationale. It does not define migration,
compatibility, or production activation.

## Design

The operator is level-triggered:

1. observe desired state, owned Kubernetes resources, and agent reports;
2. reconstruct the strongest safely provable state;
3. converge independent resources;
4. issue at most one fenced authority-changing command;
5. persist compact accepted Public V1 status;
6. re-observe until stable.

Missing or contradictory evidence produces waiting or failure, never inferred
success.

### Design Principles

- observation is authoritative;
- resource convergence is idempotent;
- commands are exact and fenced;
- status records accepted domain evidence, not an execution cursor;
- retries resolve through postconditions.

### Pure Evaluation

Decision logic consumes a complete observation and produces bounded work.
Kubernetes and agent effects remain separate from classification and planning.

### Ambiguous Effects

A lost reply never becomes success by assumption. Reconciliation re-observes
the exact command identity and durable postcondition before retrying or
advancing.

### Replica-Agent Contract

The local agent validates resource, replica, storage, process-session, epoch,
history, and operation identity before executing a command. It persists exact
authority and terminal completion before reporting success.

## Durable Boundaries

- Kubernetes stores desired resources and accepted controller status.
- The local agent stores exact lifecycle/operation authority and terminal
  completion.
- The state provider stores application state and progress.
- The Replicator owns reconstructable live transport and quorum mechanics.

The controller does not persist a workflow phase cursor for every protocol
step, and the Replicator does not persist an engine continuation log.

## Fresh Bootstrap

For `spec.replicas`, the controller creates credentials, peer/write Services,
PVCs, Pods, and exact endpoints before accepting reports. Resource convergence
is idempotent and namespaced.

A report already present in observation cannot bypass missing scaffolding.

## Safety

- exact Pod/PVC/storage/process identities fence stale work;
- epochs and PC/CC configurations fence authority;
- history identity is stronger than numeric progress;
- Primary role does not imply write access;
- one current provider writer exists only after admitted Open;
- lost command replies are resolved through durable observation;
- old or partial controller/agent/provider state rejects unchanged.

## Current API

The CRD status contains Public V1 authority, session-renewal, transition, and
operation values. The command/report protocol is version 11.

The controller is bound to one installation namespace with `Role` and
`RoleBinding` permissions.

## Excluded Designs

- migration from removed APIs or stores;
- rolling/mixed-version compatibility;
- cutover Lease or cluster-wide mutation role;
- custom release/evidence subsystem;
- export/reimport or namespace-quarantine procedure;
- transport authentication;
- production traffic activation.

Those exclusions are deliberate. Future operational rollout, if any, is a new
fresh-state project.
