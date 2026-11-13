# Kubernetes-Backed Agent Store

## Status

**Deferred proposal; not implemented.**

Kuberic currently stores local replica-agent authority in schema-12 SQLite on
the replica PVC. The Public V1 Replicator is stateless with respect to engine
continuation, but the agent remains a durable local control-plane owner.

## Possible Direction

A future design could place the agent aggregate in one namespaced Kubernetes
custom resource per storage slot. Kubernetes would then own agent authority,
while the provider continued to own application bytes and progress.

Such a design would make API-server and etcd availability part of replica
recovery and would add control-plane write load, optimistic-concurrency
handling, admission policy, and disaster-recovery requirements.

## Required Invariants

Any implementation would need:

- one atomic durable aggregate per replica/storage slot;
- exact resource, replica, storage, and process-session fencing;
- fail-closed reads and compare-and-swap updates;
- no application bytes, WAL, copy streams, or high-frequency progress in the
  API server;
- unchanged public Replicator and StateProvider interfaces;
- one metadata authority from initial provisioning.

## Explicit Non-Goals

- no migration from the current SQLite agent store;
- no dual SQLite/CRD authority mode;
- no compatibility bridge or mixed-version enrollment;
- no Kubernetes `Lease` as durable authority or execution fence;
- no claim that watches are recovery authority.

If selected, this would be a fresh-state storage architecture project, not an
operational migration for existing Kuberic replicas.

## Why It Remains Deferred

The local SQLite store already provides atomic transitions and offline
recovery without depending on the Kubernetes control plane. There is no
measured scale or availability requirement that justifies relocating it.
