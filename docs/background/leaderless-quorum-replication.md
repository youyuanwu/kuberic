# Leaderless Quorum Replication

Prior-art survey of leaderless (multi-master / masterless) quorum
replication as implemented by Amazon Dynamo, Apache Cassandra, Riak KV,
and Project Voldemort — and how it contrasts with kuberic's current
leader-based (Service Fabric-style) approach.

> **Goal:** Inform the design of a possible "leaderless quorum" mode for
> kuberic, where clients write to all replicas and replicas reach
> consensus among themselves.

---

## Table of Contents

1. [Overview](#overview)
2. [The Dynamo Paper — Foundation](#the-dynamo-paper--foundation)
3. [Apache Cassandra](#apache-cassandra)
4. [Riak KV](#riak-kv)
5. [Project Voldemort](#project-voldemort)
6. [Common Techniques](#common-techniques)
7. [Leader-Based vs. Leaderless — Comparison](#leader-based-vs-leaderless--comparison)
8. [Implications for Kuberic](#implications-for-kuberic)
9. [References](#references)

---

## Overview

In a **leader-based** system (Service Fabric, kuberic today, CNPG),
writes are accepted only by a single primary and replicated to
secondaries. This gives total ordering but makes the primary a
bottleneck and a single point of failure until failover completes.

In a **leaderless** system (Dynamo, Cassandra, Riak, Voldemort), any
replica can accept reads and writes. A **coordinator node** (often the
node that receives the client request) fans out the operation to all
replicas. The operation succeeds once a **quorum** of replicas
acknowledge. There is no distinguished leader; all nodes are
symmetrical.

```
                Leader-based (kuberic today)
                ────────────────────────────
Client ──► Primary ──► Secondary₁
                   └──► Secondary₂

                Leaderless (Dynamo-style)
                ────────────────────────
Client ──► Coordinator ──► Replica₁  ─┐
                       ├──► Replica₂  ─┤  wait for W acks
                       └──► Replica₃  ─┘
```

---

## The Dynamo Paper — Foundation

Amazon's Dynamo (2007) defined the blueprint that Cassandra, Riak, and
Voldemort all derive from. The key techniques:

| Problem | Technique | Benefit |
|---|---|---|
| Data partitioning | Consistent hashing | Incremental, linear scale-out |
| High-availability writes | Vector clocks / versioned data | Version size decoupled from update rate |
| Temporary failures | Sloppy quorum + hinted handoff | Writes succeed even when replicas are down |
| Permanent failures | Anti-entropy via Merkle trees | Proactive background divergence repair |
| Membership & failure detection | Gossip protocol | No centralized registry |

### Core Parameters: N, R, W

Every key is replicated to **N** nodes. A read requires **R**
acknowledgments; a write requires **W** acknowledgments.

- **Strong consistency:** `R + W > N` — read and write quorums
  overlap, so every read sees the latest write.
- **Typical setting:** `N=3, R=2, W=2` — tolerates 1 failure for
  both reads and writes while guaranteeing consistency.
- **High availability write:** `W=1` — writes always succeed if any
  single replica is reachable, at the cost of consistency.

### Sloppy Quorum and Hinted Handoff

A **strict quorum** counts only the N designated "home" nodes. A
**sloppy quorum** allows any reachable node to stand in for a down
replica, accepting the write and storing a **hint** (a marker that the
data belongs elsewhere). When the home node recovers, the hint is
"handed off" — the data is forwarded and the hint is deleted.

Sloppy quorum trades consistency for availability: writes can succeed
even when fewer than W home nodes are reachable, as long as W nodes
total (including stand-ins) respond. The trade-off is that a read may
not find the latest value until hinted handoff completes.

### Vector Clocks

Dynamo uses **vector clocks** to track causality. Each replica tags
mutations with `(node, counter)` pairs. On read, if one version
descends from another, the older one is discarded. If two versions are
concurrent (neither descends from the other), both are returned as
**siblings** — the client must reconcile them (semantic merge).

---

## Apache Cassandra

Cassandra (originally Facebook, now Apache) takes Dynamo's clustering
and pairs it with a BigTable-style LSM storage engine.

### Architecture

- **Peer-to-peer gossip:** All nodes are equal. Gossip protocol
  distributes ring membership, schema versions, and failure detection
  (Phi Accrual Failure Detector).
- **Consistent hashing with vnodes:** Data is partitioned onto a token
  ring. Each physical node owns multiple virtual nodes (vnodes) for
  balanced distribution.
- **Replication strategies:** `SimpleStrategy` (single ring walk) or
  `NetworkTopologyStrategy` (rack-/DC-aware placement).

### Write Path

1. Client sends write to any node — that node becomes the
   **coordinator**.
2. Coordinator determines the N replicas via the token ring.
3. Coordinator forwards the mutation to **all N replicas** in parallel.
4. Each replica writes to its commit log + memtable and ACKs.
5. Coordinator responds to client after receiving **W** ACKs.

Write operations are always sent to all replicas regardless of the
consistency level. The consistency level only controls how many
responses the coordinator waits for.

### Read Path

1. Coordinator sends full data requests to enough replicas to satisfy
   **R**.
2. If `R < N`, the coordinator sends **digest requests** to additional
   replicas (hash only) to detect inconsistencies.
3. If a mismatch is detected, a **read repair** is triggered in the
   background.

### Conflict Resolution

Cassandra **simplified Dynamo's vector clock approach** by adopting
**Last-Write-Wins (LWW)** based on client-supplied or coordinator
timestamps. Each CQL column is independently timestamped. The latest
timestamp wins.

Formally, Cassandra models each CQL row as an **LWW-Element-Set CRDT**.
This means:
- Concurrent updates to *different* columns within a row are
  conflict-free.
- Concurrent updates to the *same* column are resolved by timestamp.
- Correctness depends on reasonably synchronized clocks (NTP).

### Consistency Levels

| Level | Meaning |
|---|---|
| `ONE` | 1 replica ACKs |
| `QUORUM` | ⌊N/2⌋+1 replicas ACK |
| `ALL` | All N replicas ACK |
| `LOCAL_QUORUM` | Majority in local DC |
| `EACH_QUORUM` | Majority in every DC |
| `ANY` | 1 replica or coordinator stores a hint |

### Anti-Entropy

- **Read repair:** Background repair triggered on read when digest
  mismatch is detected.
- **Full / incremental repair:** Offline Merkle-tree comparison.
  Incremental repair (post-2.1) tracks only unrepaired SSTables.
- **Hinted handoff:** Coordinator stores hints for unreachable replicas
  and replays them when the node returns.

---

## Riak KV

Riak (Basho Technologies, now community-maintained) is the most
faithful open-source implementation of Dynamo.

### Architecture

- **Masterless ring:** All nodes identical, no distinguished roles.
- **Consistent hashing + vnodes:** 64-partition ring by default.
  `n_val` (default 3) controls replication factor.
- **Erlang/OTP:** Each vnode is an Erlang process (`riak_kv_vnode`).
  A `riak_kv_vnode_master` routes requests to per-partition processes.

### Write Path

1. Client sends write to any node (coordinator).
2. Coordinator hashes `{bucket, key}` → token → finds N preferred
   partitions via ring walk.
3. Coordinator sends put to all N partition owners.
4. If a home node is down, the next node in the preference list acts as
   a **fallback** — sloppy quorum.
5. Coordinator waits for **W** ACKs (configurable per-request).

### Conflict Resolution — Vector Clocks and DVVs

Unlike Cassandra, Riak preserves Dynamo's causal-context approach:

- **Vector clocks** (pre-2.0): `(actor, counter)` pairs track update
  causality. Concurrent writes produce **siblings** — the application
  must resolve them.
- **Dotted version vectors** (2.0+): Each value is tagged with a *dot*
  (minimal vector clock) identifying the specific event that created it.
  This prevents sibling explosion from duplicate values.
- **CRDTs** (Riak Data Types): Maps, sets, counters, flags, registers
  with built-in merge semantics — no application-side conflict
  resolution needed.

### Anti-Entropy

- **Read repair:** On successful read, if any responding vnode has a
  stale or missing replica, the coordinator triggers a background
  repair.
- **Active anti-entropy (AAE):** Continuous background Merkle-tree
  exchange between replicas. Uses persistent on-disk hash trees (not
  in-memory) so trees survive restarts. Trees are updated in real-time
  as writes arrive and periodically regenerated from disk to catch
  silent corruption.
- **Hinted handoff + hometest:** Fallback vnodes periodically check
  whether the home node is back. If so, they transfer data and shut
  down.

### Tunable Consistency

Riak 2.0+ added optional **strong consistency** (per-bucket) using a
consensus protocol alongside the eventual-consistency default. This is
effectively a leader-based approach layered on top of the leaderless
infrastructure.

---

## Project Voldemort

Voldemort (LinkedIn, now largely superseded by Venice) is a Java-based
Dynamo clone focused on being a "big, distributed, persistent hash
table." It is notable for its **client-side routing** mode, where the
quorum logic runs entirely in the client library — the client sends
writes directly to all N replicas and waits for W ACKs itself, with no
server-side coordinator.

### Layered Architecture

Voldemort's code is organized as a stack of composable layers, each
implementing a simple `Store` interface (`get`, `put`, `delete`):

```
┌────────────────────────────────┐
│  Conflict Resolution           │  InconsistencyResolver
├────────────────────────────────┤
│  Serialization                 │  key/value → bytes
├────────────────────────────────┤
│  Routing  ← can sit here      │  RoutedStore / PipelineRoutedStore
├────────────────────────────────┤
│  Network (TCP/HTTP)            │  per-node Store handles
├────────────────────────────────┤
│  Routing  ← or here           │  (server-side routing)
├────────────────────────────────┤
│  Persistence                   │  BDB-JE, MySQL, memory, read-only
└────────────────────────────────┘
```

The key insight is that the **routing layer can be placed above or
below the network layer**, controlled by a per-store `<routing>`
setting:

- **`<routing>client</routing>`** — Client-side routing. The client
  library contains the full routing logic. Writes go directly from
  client to all N replica nodes. This is the **1-hop** path.
- **`<routing>server</routing>`** — Server-side routing. The client
  sends to any node (via load balancer), which then re-routes to the
  correct replicas. This is the **2-hop** path.

### Client-Side Routing — How It Works

When `routing=client`, the client library's `PipelineRoutedStore`
performs the full quorum protocol:

**Write path:**
1. Client hashes the key → determines N replica nodes via
   `RoutingStrategy` (consistent hashing on the token ring).
2. Client sends the put to **all N replicas in parallel**
   (`PerformParallelPutRequests`).
3. Client waits for **`required-writes`** (W) ACKs — returns success.
4. If `preferred-writes` > `required-writes`, client continues waiting
   for more ACKs (best-effort), but has already returned to the app.
5. If a replica is down, client stores a **hint** in a slop store
   (`PerformPutHintedHandoff`) for later replay.

**Read path:**
1. Client sends get to replicas, waiting for **`required-reads`** (R).
2. If multiple conflicting versions come back (vector clock siblings),
   the client runs an `InconsistencyResolver` to pick one.
3. If `repairReads` is enabled, client triggers **read repair** —
   sends the resolved value back to out-of-date replicas.

**Failure detection:** The client library runs its own failure detector
to avoid sending requests to known-dead nodes.

### Store Configuration — Tunable Quorum

Each store is configured with independent quorum parameters in
`stores.xml`:

```xml
<store>
    <name>test</name>
    <replication-factor>3</replication-factor>     <!-- N -->
    <preferred-reads>2</preferred-reads>           <!-- try R=2 -->
    <required-reads>1</required-reads>             <!-- min R=1 -->
    <preferred-writes>2</preferred-writes>         <!-- try W=2 -->
    <required-writes>1</required-writes>           <!-- min W=1 -->
    <persistence>bdb</persistence>
    <routing>client</routing>                      <!-- CLIENT-SIDE -->
    <routing-strategy>consistent-routing</routing-strategy>
    ...
</store>
```

The `preferred-*` vs `required-*` distinction is unique to Voldemort:
- **`required-*`:** Minimum ACKs for the operation to succeed. If
  fewer than this respond, the client throws an exception.
- **`preferred-*`:** Ideal number of ACKs. The client tries to reach
  this many but accepts `required-*` as sufficient.

This allows expressing "I want 2 ACKs ideally, but 1 is acceptable"
without blocking indefinitely.

### Vector Clock Versioning

Voldemort preserves Dynamo's full vector clock model:

1. Client does `get(key)` → receives `Versioned<V>` containing the
   current vector clock.
2. Client modifies the value.
3. Client does `put(key, versioned)` — the vector clock from step 1 is
   sent back, and the receiving node increments its own counter.
4. On read, if two versions are concurrent (neither descends from the
   other), both are returned. The client's `InconsistencyResolver`
   must pick one.

This is the full read-modify-write cycle from the Dynamo paper,
implemented entirely in the client library.

### Why Client-Side Routing?

From Voldemort's design docs:

> "Fewer hops is nice from a latency perspective (since there are fewer
> hops), nice from a throughput perspective (since there are fewer
> potential bottlenecks), but requires the routing intelligence to move
> up the stack."

The trade-offs:

| | Client-side (`routing=client`) | Server-side (`routing=server`) |
|---|---|---|
| **Latency** | 1 hop — lowest | 2 hops — higher |
| **Throughput** | No coordinator bottleneck | Coordinator can bottleneck |
| **Client complexity** | Fat client (Java library required) | Thin client (any HTTP client) |
| **Topology awareness** | Client must know the ring | Client is topology-unaware |
| **Client language** | Java only (or reimplement) | Any language via HTTP |
| **Failure detection** | Client-side (per-client overhead) | Server-side (shared) |

LinkedIn used `routing=client` for their Java services (the common
case), and `routing=server` behind a hardware load balancer for
non-Java clients or simple tooling.

### Client State Model — No Persistent State

A critical property of Voldemort's client-side routing: **the client
holds no persistent state**. Everything is in-memory and
reconstructable from servers.

**Bootstrap sequence:**
1. Client is configured with **bootstrap URLs** — addresses of any
   Voldemort server nodes.
2. On startup, client fetches `cluster.xml` (node topology, partition
   assignments) and `stores.xml` (store configs including N/R/W) from
   any bootstrap node via a metadata key lookup.
3. Client builds the consistent hash ring in memory from `cluster.xml`.
4. Client opens socket connections to all relevant nodes.
5. If metadata becomes stale (server returns
   `InvalidMetadataException`), the client re-bootstraps automatically
   — re-fetches `cluster.xml` and `stores.xml`, rebuilds the ring,
   reconnects. This happens transparently on the next retry.

**What the client holds in memory:**

| State | Persistent? | How recovered |
|---|---|---|
| Cluster topology (ring) | No | Re-fetch `cluster.xml` from any node |
| Store configs (N/R/W) | No | Re-fetch `stores.xml` from any node |
| Socket connection pool | No | Reconnect to nodes |
| Failure detector state | No | Reset (assume all alive, re-probe) |
| Vector clocks | No | Per-request: fetched on `get`, sent back on `put` |
| Hinted handoff (slops) | No | Slops stored **server-side** on stand-in nodes |

**Why no persistent state is needed:**
- **No LSN assignment:** Voldemort uses vector clocks, which are
  incremented by the **server** (the node receiving the write), not
  the client. The client sends the current vector clock with each
  `put`; the server increments its own counter in the clock.
- **No replication queue:** The client sends to all N replicas in
  parallel. There is no "buffer ops for replay to new replicas"
  concept. If a node is down, the client writes a **slop** (hint) to
  another live node (`PerformPutHintedHandoff`). The slop is stored
  **server-side** — not in the client. Handoff is also server-side.
- **No epoch/fencing:** There is no single-writer constraint. Any
  client can write to any key at any time. Conflicts are resolved by
  vector clocks at read time, not by preventing concurrent writes.
- **No coordinator state:** The client is a stateless coordinator —
  it routes, waits for quorum, and returns. No state carries over
  between requests.

**Client crash recovery:** If the client process crashes and
restarts, it simply re-bootstraps from the bootstrap URLs. No state
is lost because no client state was authoritative — the servers are
the source of truth for data, versions, and topology. The new client
instance is indistinguishable from the old one.

This stateless client model is possible because Dynamo-family systems
use **partial ordering** (vector clocks) rather than **total
ordering** (sequential LSNs). Total ordering requires a centralized
counter, which implies state. Partial ordering distributes version
tracking to the servers, keeping the client stateless.

### Key Design Points (Summary)

- **Consistent hashing** with configurable replication factor.
- **Vector clock versioning:** Concurrent updates produce multiple
  versions; application resolves (not LWW).
- **Pluggable storage:** BDB-JE, MySQL, in-memory, read-only
  (batch-computed data from Hadoop).
- **Pluggable serialization:** Avro, Protocol Buffers, Thrift, Java
  serialization.
- **In-memory caching:** Eliminates separate caching tier.
- **Hinted handoff:** Client-side (in `routing=client` mode) or
  server-side, with pluggable strategies (`proximity-handoff`,
  `consistent-handoff`, `any-handoff`).
- **Zone-aware routing:** `zone-routing` strategy with per-zone
  replication factors and `zone-count-reads`/`zone-count-writes`
  to control cross-DC blocking behavior.
- **AP system** in CAP terms — prioritizes availability and partition
  tolerance.

### Fate

LinkedIn migrated off Voldemort circa 2018. In 2022 they open-sourced
**Venice** as a replacement, a derived-data platform rather than a
general-purpose KV store. Voldemort is effectively unmaintained, but
its client-side routing architecture remains a unique and instructive
design point in the Dynamo family.

---

## Common Techniques

All Dynamo-family systems share these core mechanisms:

### 1. Consistent Hashing and Token Ring

Data is mapped to a position on a hash ring. The N nodes clockwise from
that position are the replicas. Adding or removing a node only affects
neighboring ranges — most data stays put.

### 2. Quorum Reads and Writes (R + W > N)

The fundamental consistency knob. By tuning R and W per-request,
applications trade latency and availability for consistency:

```
  Strong:   R=2, W=2, N=3  →  R+W=4 > 3  ✓
  Fast read: R=1, W=3, N=3  →  R+W=4 > 3  ✓ (reads fast, writes slow)
  Fast write: R=3, W=1, N=3  →  R+W=4 > 3  ✓ (writes fast, reads slow)
  Eventual: R=1, W=1, N=3  →  R+W=2 < 3  ✗ (may read stale)
```

### 3. Hinted Handoff

When a designated replica is unreachable, a stand-in node accepts the
write and stores a hint. Once the home node recovers, the hint is
replayed and deleted. This enables writes during partial failures.

### 4. Anti-Entropy Repair

Background Merkle-tree exchange between replicas to detect and fix
divergent data. Catches issues that read repair and hinted handoff miss
(e.g., cold data, bit rot).

### 5. Conflict Resolution Strategies

| Strategy | Used by | Pro | Con |
|---|---|---|---|
| **LWW (timestamp)** | Cassandra | Simple, no app logic needed | Clock skew → silent data loss |
| **Vector clocks** | Dynamo, Voldemort | Detects true conflicts | Metadata grows, app must resolve |
| **Dotted version vectors** | Riak 2.0+ | Prevents sibling explosion | Slightly more complex than VC |
| **CRDTs** | Riak Data Types | Automatic merge, no conflicts | Limited to specific data structures |
| **Application merge** | Dynamo (shopping cart) | Semantic correctness | Pushes complexity to app layer |

### 6. Gossip-Based Membership

No master registry. Nodes periodically exchange state (ring membership,
load, schema versions) via gossip protocol. Failure detection is
decentralized (Phi Accrual in Cassandra, heartbeat-based in Riak).

---

## Leader-Based vs. Leaderless — Comparison

| Dimension | Leader-based (SF / kuberic) | Leaderless (Dynamo-family) |
|---|---|---|
| **Write ordering** | Total order from primary | No global order; per-key causal or LWW |
| **Write availability** | Blocked during failover | Any reachable quorum can accept writes |
| **Read consistency** | Strong (read from primary) | Tunable (R+W>N for strong, R=1 for fast) |
| **Conflict resolution** | None needed (single writer) | Required (LWW, vector clocks, CRDTs) |
| **Failover** | Explicit reconfiguration | Transparent — sloppy quorum + hinted handoff |
| **Write amplification** | 1 → N (primary fans out) | 1 → N (coordinator fans out) — similar |
| **Complexity** | Reconfigurator / epoch / fencing | Consistent hashing / gossip / anti-entropy |
| **Suitable for** | Strong consistency, ordered logs | High availability, multi-DC, last-write-wins |
| **Transactions** | Possible (single-partition) | Very limited (Cassandra LWT, Riak strong consistency) |

### Key Trade-Off

Leaderless systems excel at **always-on writes** — there is no failover
delay because there is no leader to fail over. The price is weaker
consistency guarantees: concurrent writes to the same key can diverge,
requiring conflict resolution. Leader-based systems provide **strong
ordering** but have a window of unavailability during leader election.

---

## Implications for Kuberic

Kuberic today is firmly leader-based (SF-style epoch/quorum/fencing).
Adding a leaderless mode would require:

### What Would Change

1. **Coordinator role:** Two options exist, as Voldemort demonstrated:
   - **Server-side coordinator:** Any pod could coordinate writes.
     The client sends to one pod, which fans out to all replicas and
     waits for W ACKs. Simpler clients but adds a network hop.
   - **Client-side coordinator (Voldemort model):** The client library
     knows the replica set and sends writes directly to all N pods,
     waiting for W ACKs itself. Lowest latency, no coordinator
     bottleneck, but requires a "fat" client with topology awareness.

2. **Conflict resolution:** The `StateProvider` trait would need to
   support either LWW (timestamp-based) or semantic merge (CRDTs /
   application-defined). The current WAL-based replicator assumes
   total ordering via a primary — this wouldn't apply.

3. **No epoch/fencing:** Without a single writer, epoch-based fencing
   is unnecessary. Instead, versioning (vector clocks or LWW
   timestamps) determines data currency.

4. **Anti-entropy:** A background Merkle-tree comparison process would
   replace the current copy-based replica building. Read repair could
   also be added.

5. **Hinted handoff:** Pods would need to accept writes on behalf of
   temporarily down peers and replay them later.

6. **Consistent hashing / partitioning:** For sharded deployments, a
   token ring or equivalent partitioning scheme would be needed. For
   single-partition use cases (kuberic's current model), this is less
   relevant.

### What Would Stay

- **Kubernetes operator / CRD:** Cluster topology management.
- **gRPC data plane:** Transport between replicas.
- **Pod lifecycle:** Managed by the operator, but without primary
  election.

### Design Questions

- **Scope:** Full Dynamo-style with consistent hashing, or just
  leaderless quorum for a single partition (all replicas hold all data)?
- **Conflict resolution default:** LWW (simpler, Cassandra-style) or
  vector clocks / CRDTs (safer, Riak-style)?
- **Backward compatibility:** Can leaderless mode coexist with the
  current leader-based mode, selected per-partition?
- **Consistency model:** Should kuberic expose tunable R/W/N or just
  offer preset levels (ONE, QUORUM, ALL)?

---

## References

- DeCandia, G. et al. (2007). *Dynamo: Amazon's Highly Available
  Key-value Store.* SOSP '07.
  [Paper (Cornell)](https://www.cs.cornell.edu/courses/cs5414/2017fa/papers/dynamo.pdf)
- Apache Cassandra. *Dynamo Architecture.*
  [cassandra.apache.org/doc/latest/cassandra/architecture/dynamo.html](https://cassandra.apache.org/doc/latest/cassandra/architecture/dynamo.html)
- Riak KV Documentation. *Replication.*
  [docs.riak.com/riak/kv/latest/learn/concepts/replication](https://docs.riak.com/riak/kv/latest/learn/concepts/replication/index.html)
- Riak KV Documentation. *Causal Context (Vector Clocks & DVVs).*
  [docs.riak.com/riak/kv/latest/learn/concepts/causal-context](https://docs.riak.com/riak/kv/latest/learn/concepts/causal-context/index.html)
- Riak KV Documentation. *Active Anti-Entropy.*
  [docs.riak.com/riak/kv/latest/learn/concepts/active-anti-entropy](https://docs.riak.com/riak/kv/latest/learn/concepts/active-anti-entropy/index.html)
- Project Voldemort. *Design.*
  [web.archive.org/web/2018/project-voldemort.com/voldemort/design.html](https://web.archive.org/web/2018/http://www.project-voldemort.com/voldemort/design.html)
  (archived)
- Project Voldemort. *Configuration (stores.xml, routing modes).*
  [web.archive.org/web/2018/project-voldemort.com/voldemort/configuration.html](https://web.archive.org/web/20181226070441/http://www.project-voldemort.com/voldemort/configuration.html)
  (archived)
- Project Voldemort source: `PipelineRoutedStore.java` — client-side
  routing with parallel put/get and read repair.
  [github.com/voldemort/voldemort/.../PipelineRoutedStore.java](https://github.com/voldemort/voldemort/blob/master/src/java/voldemort/store/routed/PipelineRoutedStore.java)
- LinkedIn Engineering (2022). *Open-Sourcing Venice.*
  [engineering.linkedin.com/blog/2022/open-sourcing-venice](https://engineering.linkedin.com/blog/2022/open-sourcing-venice--linkedin-s-derived-data-platform)
- Kleppmann, M. (2017). *Designing Data-Intensive Applications.*
  O'Reilly Media. Chapters 5 (Replication) and 6 (Partitioning).
