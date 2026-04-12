# kuberic-core

Core replication framework for [Kuberic](../README.md). Provides the building blocks for stateful replicated services on Kubernetes.

## Key Components

| Module | Purpose |
|--------|---------|
| `PodRuntime` | Per-pod async runtime — hosts the replicator and dispatches lifecycle events to user code |
| `WalReplicator` | Quorum-based replicator — manages primary→secondary data flow, copy protocol, epoch tracking |
| `PartitionDriver` | Orchestrates replica lifecycle — create partition, failover, switchover, scale up/down |
| `ReplicaHandle` | Trait abstracting gRPC (production) vs in-process (testing) replica communication |

## Usage

Your service receives lifecycle events and implements a state provider:

```rust
// In your Open handler:
let (handle, handles) = WalReplicator::create(id, &data_bind, fault_tx, sp_tx).await?;

// On ChangeRole(Primary):
// Start your client-facing gRPC server

// On replication stream (secondary):
// Persist data, then acknowledge
op.acknowledge();
```

See [examples/kvstore](../examples/kvstore/) and [examples/sqlite](../examples/sqlite/) for complete implementations.

## Replication Model

- **Primary** serializes operations, calls `replicator.replicate()`, blocks until write quorum ACKs
- **Secondary** receives operations via gRPC data plane, persists, then acknowledges
- **Copy protocol** builds new replicas from full snapshot + incremental replay
- **Epoch fencing** rejects stale operations from old primaries after failover
