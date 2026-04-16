# Kuberic: Peer-Repair Replication

> **Status: ABANDONED.** This design does not improve write latency —
> the original motivation. Both peer-repair and writer-based designs
> wait for W ACKs, yielding the same write latency. Writer-based
> actually has better tail latency: it sends to all N and waits for
> the W-th fastest ACK, naturally dodging slow replicas. Peer-repair
> sends to exactly W and must wait for the slowest of that subset.
>
> Peer-repair's actual benefits (stateless Writer, reduced Writer
> bandwidth) do not justify its costs: significantly higher replica
> complexity (repair protocol, op store, GC coordination, 6 known
> issues) and worse tail latency. The committed_lsn tracking
> complexity (coordinator persistence, durability trade-offs) is
> identical in both designs and cannot be avoided.
>
> The writer-based design (`writer-based-replication.md`) was also
> subsequently abandoned — separating the Writer from replicas creates
> a `committed_lsn` problem whose correct solution (coordinator +
> etcd persistence) adds back the latency that was saved. The existing
> leader-based operator (`kuberic-operator`) remains the optimal
> design for write latency with correct semantics.
>
> The analysis below is preserved for reference.

---

Design for a replication system where the Writer sends writes to a
quorum of replicas, and replicas **repair each other** to fill in
missing data. The Writer's job ends once W replicas ACK.
Replicas that missed the write (because they weren't in the W) or
were temporarily down catch up by pulling from peers — not from the
Writer.

This is a separate system from both the existing leader-based
operator (`kuberic-operator`) and the Writer-based design
(`writer-based-replication.md`).

> **Scope:** Single writer only. Multi-writer extensions are out of
> scope.

> Part of the [Kuberic Design](../kuberic-replicator-design.md).
> Related: [Writer-Based Replication](writer-based-replication.md),
> [Leaderless Quorum Research](../../background/leaderless-quorum-replication.md),
> [Lazy Repair Research](../../background/lazy-repair-replication.md)

---

## Motivation

### The Problem with Writer-Based Replication

In the Writer-based design, the Writer sends every op to **all N
replicas** and waits for W ACKs. This has two costs:

1. **The Writer fans out to all N replicas**, even though only W ACKs
   are needed. The N-W extra sends consume Writer bandwidth and add
   tail-latency sensitivity (the Writer's `send_to_all` must reach
   all N).

2. **The Writer holds a `ReplicationQueue`** to replay ops to new
   replicas during builds. If the Writer crashes during a build, the
   queue is lost (KI-2). The queue also grows unboundedly during
   builds (KI-4).

### The Peer-Repair Alternative

What if the Writer only sends to W replicas (the quorum), and the
remaining N-W replicas catch up from their peers?

```
Writer-based (current design):        Peer-repair-based:
─────────────────────────────          ──────────────────────

Writer ──► Replica₁ (ACK)             Writer ──► Replica₁ (ACK)
       ──► Replica₂ (ACK)                    ──► Replica₂ (ACK)
       ──► Replica₃ (no ACK needed)          (done — Replica₃ catches up later)
                                                         │
                                       Replica₃ ◄── pull from Replica₁ or Replica₂
```

**Writer becomes simpler:**
- No `send_to_all` — Writer sends to W replicas, waits for W ACKs,
  returns LSN. Done.
- No `ReplicationQueue` — Writer doesn't need to buffer ops for
  replay. Replicas hold their own data and share it with peers.
- Writer crash has less impact — no queue to lose. Replicas
  repair among themselves.

**Replicas become smarter:**
- Each replica knows what LSNs it has and what it's missing.
- Replicas periodically compare state with peers and pull missing ops.
- This is the **anti-entropy** model from Dynamo/Cassandra/Riak,
  adapted for totally-ordered single-writer ops.

---

## Prior Art

### Dynamo-Family Anti-Entropy

All Dynamo-family systems (Cassandra, Riak, Voldemort) use
replica-to-replica repair as a core mechanism:

| Mechanism | When | How |
|---|---|---|
| **Read repair** | On read | Coordinator compares digests from R replicas; if mismatch, pushes latest value to stale replicas |
| **Hinted handoff** | On write failure | Stand-in node stores a "hint" for the down replica; replays when it recovers |
| **Active anti-entropy (AAE)** | Background | Replicas exchange Merkle tree hashes; identify and repair divergent/missing keys |

Key differences from our design:
- Dynamo systems are **multi-writer** — any node can accept writes,
  so conflicts are possible. They use vector clocks / LWW for
  conflict resolution.
- Our system is **single-writer** with **total ordering** (LSNs).
  No conflicts are possible. Repair is simpler: a replica
  either has an op or it doesn't. No merge logic needed.

### Cassandra Read Repair and Anti-Entropy Repair

Cassandra's write path sends to all N replicas but only waits for W
ACKs (same as Writer-based). Replicas that missed the write are
healed by:

1. **Read repair:** On read, coordinator sends digest requests to
   extra replicas. If a replica is behind, the coordinator pushes
   the latest value.
2. **Anti-entropy repair:** Background Merkle tree comparison between
   replicas. Runs periodically (operator-initiated or scheduled).
   Identifies and fixes missing or divergent data.

### Riak Active Anti-Entropy (AAE)

Riak's AAE runs continuously as a background process:

1. Each vnode maintains a **persistent on-disk Merkle tree** (hash
   tree) of its key/value data.
2. Trees are updated in real-time as writes arrive.
3. Periodically, vnodes exchange tree hashes with their replicas.
4. Mismatches are drilled down level-by-level until the specific
   divergent keys are found.
5. The vnode with the more recent value pushes it to the stale vnode.

Key advantage: AAE works for "cold data" that is never read —
read repair alone can't fix data that nobody reads.

### Epidemic / Gossip Protocols

The foundational paper: Demers et al. (1987), "Epidemic Algorithms
for Replicated Database Maintenance." Defines two modes:

- **Anti-entropy:** Each node periodically picks a random peer and
  exchanges state differences. Guaranteed convergence but slower.
- **Rumor mongering:** Each node that learns new data actively pushes
  it to random peers until the rumor "dies" (enough peers already
  know). Faster but not guaranteed.

Our peer-repair design uses **anti-entropy** (periodic pairwise
exchange) because we need guaranteed convergence, not just
probabilistic spread.

### Log-Based Peer-Repair (Unique to Our Design)

Unlike Dynamo-family systems which repair **key-value pairs**
(comparing individual keys), our system repairs a **totally
ordered log**. This is simpler:

- Each replica knows its `received_lsn` — the highest contiguous LSN
  it has.
- To catch up, a replica asks any peer: "give me ops from LSN X+1
  onward."
- No Merkle trees needed for the common case — a single integer
  comparison (`received_lsn`) tells a replica exactly what it's
  missing.
- Merkle trees are only needed for detecting **corruption** (a
  replica thinks it has LSN 50 but the data is wrong), not for
  detecting **gaps** (which LSN comparison handles).

---

## Design

### Architecture

```
User App + Writer                     In-Cluster
─────────────────                     ──────────

Writer ──gRPC──► Replica₁  ◄──repair──► Replica₂
       ──gRPC──► Replica₂  ◄──repair──► Replica₃
                 Replica₃  ◄──repair──► Replica₁
                    │
                    ▼
              Coordinator ◄── watches ── Operator/CRD
```

**Three distinct data flows:**

1. **Write path** (Writer → W replicas): Writer sends to W replicas,
   waits for W ACKs. Fast, minimal.
2. **Repair path** (replica ↔ replica): Background process
   where replicas compare `received_lsn` and pull missing ops from
   peers. Continuous, asynchronous.
3. **Control path** (Coordinator ↔ Writer): Same as Writer-based
   design — epoch, config updates, lifecycle.

### Write Path (Simplified)

```rust
impl Writer {
    pub async fn replicate(&self, data: Bytes) -> Result<Lsn> {
        let lsn = self.next_lsn.fetch_add(1, Ordering::Relaxed);

        // Send to W replicas (not all N)
        let targets = self.pick_w_replicas();
        let mut acks = 0;
        let futures = targets.iter().map(|r| r.send(lsn, &data));

        for result in join_all(futures).await {
            if result.is_ok() { acks += 1; }
        }

        if acks >= self.write_quorum {
            Ok(lsn)
        } else {
            Err(KubericError::NoWriteQuorum)
        }
    }
}
```

**Key difference from Writer-based design:** No `send_to_all`. No
`ReplicationQueue`. The Writer sends to exactly W replicas and
forgets about the op. The remaining N-W replicas will get the op
via peer-repair.

**Replica selection:** The Writer can use round-robin, random, or
latency-based selection to pick which W replicas receive each write.
This naturally load-balances writes across replicas.

### Peer-Repair Protocol

Each replica runs a background repair loop:

```
Every T seconds (e.g., 100ms):
  1. Pick a random peer replica
  2. Send PeerRepairRequest { my_received_lsn, my_committed_lsn }
  3. Peer responds with:
     - peer_committed_lsn
     - ops from my_received_lsn+1 to peer_committed_lsn
       (ONLY committed ops — see Committed-Only Rule below)
  4. Apply received ops to local state
  5. Update my received_lsn and committed_lsn
```

```protobuf
service ReplicaPeerRepair {
    // Pairwise peer-repair: compare LSNs, exchange missing ops
    rpc PeerRepair(PeerRepairRequest) returns (PeerRepairResponse);
}

message PeerRepairRequest {
    int64 my_received_lsn = 1;     // requester's highest contiguous LSN
    int64 my_committed_lsn = 2;    // requester's committed LSN
    int64 writer_epoch = 3;        // requester's current writer_epoch
    int64 data_loss_number = 4;    // requester's current data_loss_number
}

message PeerRepairResponse {
    int64 peer_committed_lsn = 1;  // responder's committed LSN
    repeated ReplicationItem ops = 2;  // committed ops the requester is missing
    int64 writer_epoch = 3;        // responder's current writer_epoch
    int64 data_loss_number = 4;    // responder's current data_loss_number
    int64 peer_oldest_available_lsn = 5; // lowest LSN still in op store (for snapshot trigger)
}
```

##### Epoch-Aware Peer-Repair

Replicas must validate epochs during repair to prevent
stale-epoch peers from re-injecting rolled-back ops:

```
On receiving PeerRepairRequest:
  if requester.writer_epoch < my.writer_epoch
     OR requester.data_loss_number < my.data_loss_number:
    → REJECT: respond with my epoch, no ops
       (requester learns it's stale and must re-sync with coordinator)

  if requester.writer_epoch > my.writer_epoch:
    → REJECT: I'm stale, don't send my ops
       (I need to re-sync with coordinator first)

  if epochs match:
    → PROCEED: exchange committed ops normally
```

This prevents the scenario where a partitioned replica at
`writer_epoch=1` re-injects rolled-back ops into a peer at
`writer_epoch=2`. The stale peer's repair request is
rejected, and it learns it needs to contact the coordinator for
the current epoch before it can participate in repair.

##### The Committed-Only Repair Rule

**Peer-repair must only propagate committed ops** — ops with
LSN ≤ `committed_lsn`. Uncommitted ops (above `committed_lsn`)
must NOT spread via peer-repair.

```
Repair response filter:
  Send ops WHERE lsn > requester_received_lsn
             AND lsn <= my_committed_lsn

  NOT: lsn <= my_received_lsn  ← WRONG (would spread uncommitted ops)
```

**Why this matters — the uncommitted op scenario:**

```
N=3, W=2. Writer sends op LSN=50 to Replica₁ and Replica₂.

Case A: Normal — both ACK, Writer commits.
  committed_lsn = 50.
  Replica₃ repairs from either peer, gets LSN=50. ✓

Case B: Writer crashes after sending to Replica₁ only.
  Replica₁ has LSN=50 (received, NOT committed — Writer never got W ACKs)
  Replica₂ does NOT have LSN=50
  Replica₃ does NOT have LSN=50
  committed_lsn = 49 (last committed)

  WITHOUT committed-only rule:
    Replica₃ repairs with Replica₁
    Replica₁ sends LSN=50 (it has it, received_lsn=50)
    Replica₃ applies LSN=50
    → Uncommitted op has spread! When new Writer starts and epoch
      bumps, Replica₁ rolls back LSN=50 but Replica₃ now has it.
      Divergent state. ✗

  WITH committed-only rule:
    Replica₃ repairs with Replica₁
    Replica₁ responds with ops up to committed_lsn=49
    LSN=50 is NOT sent (it's above committed_lsn)
    Replica₃ stays at received_lsn=49 ✓
    When epoch bumps, Replica₁ rolls back LSN=50. Clean. ✓
```

The rule ensures that peer-repair is safe even during Writer
failure. Only quorum-confirmed ops propagate between replicas.
Uncommitted ops remain isolated on the replicas that received them
directly from the Writer, and are rolled back on epoch change.

##### One-Replica-Surviving Scenario

```
N=3, W=2. Writer sends LSN=50 to Replica₁ and Replica₂. Both ACK.
committed_lsn = 50. Writer returns success to user.

Replica₂ dies BEFORE repair to Replica₃.

Surviving:
  Replica₁: has LSN=50, committed_lsn=50
  Replica₃: does NOT have LSN=50, committed_lsn=49

Is the op safe? YES:
  - It was quorum-committed (W=2 ACKed before Replica₂ died)
  - committed_lsn=50 proves it was quorum-confirmed
  - Replica₃ repairs with Replica₁
  - Replica₁ sends LSN=50 (it's ≤ committed_lsn=50) ✓
  - Replica₃ applies it

No ambiguity — committed_lsn is the proof of quorum commitment.
A single surviving copy of a committed op is sufficient because
committed_lsn confirms it was quorum-confirmed at write time.
The op doesn't need to exist on W replicas at repair time;
it only needed W ACKs at write time.

Compare with leader-based system: if the primary dies and only one
secondary has the latest ops, failover promotes that secondary.
The mechanism is different (promotion vs. peer-repair) but the
invariant is the same: a committed op survives as long as ANY
replica with that op survives.
```

##### All-Copies-Lost Scenario (Data Loss)

```
N=3, W=2. Writer sends LSN=50 to Replica₁ and Replica₂. Both ACK.
committed_lsn = 50.

Both Replica₁ AND Replica₂ die. Only Replica₃ survives.
Replica₃ has committed_lsn=49, does NOT have LSN=50.

LSN=50 is LOST — no surviving replica has it.
This is quorum loss → data loss — same as in any replication system.
Operator must bump data_loss_number and run data loss recovery
(same protocol as Writer-based design: pick canonical survivor,
call on_data_loss(), rebuild others from canonical).
```

This is identical to the leader-based system: if both the primary
and the one secondary that had the latest op die, the op is lost.
Quorum replication guarantees survival through F failures (where
F = N - W). It does not guarantee survival through F+1 failures.

**Properties:**

- **Convergence guaranteed:** As long as any two replicas can
  eventually communicate, all replicas will converge to the same
  committed state. This follows from epidemic algorithm theory —
  pairwise random exchange converges in O(log N) rounds.
- **Epoch-aware:** Peer-repair validates `writer_epoch` and
  `data_loss_number` on every exchange. Stale-epoch peers are
  rejected, preventing re-injection of rolled-back ops.
- **No uncommitted propagation:** The committed-only rule prevents
  uncommitted ops from spreading, avoiding divergent state after
  Writer crash.
- **No coordinator involvement:** Peer-repair is peer-to-peer,
  no central coordinator needed.
- **Idempotent:** Receiving an op you already have is a no-op
  (check LSN, skip if already applied).
- **Bounded catch-up time:** For N=3 with 100ms repair
  interval, a lagging replica catches up within ~200-300ms
  (1-2 rounds of gossip).

### Replica Data Model

Each replica maintains:

```rust
struct ReplicaState {
    /// Ops stored locally: LSN → data
    /// Retains ops for peer-repair
    ops: BTreeMap<Lsn, Bytes>,
    /// Highest contiguous LSN received (no gaps below this)
    received_lsn: Lsn,
    /// Highest committed LSN (from Writer via ReplicationItem)
    committed_lsn: Lsn,
    /// Applied to user state up to this LSN
    applied_lsn: Lsn,
}
```

**Ops retention:** Replicas retain ops in their local store for
peer-repair. Ops are GC'd once all N replicas have confirmed
receipt (via repair exchanges). This replaces the Writer's
`ReplicationQueue`.

**GC protocol:** During repair, replicas exchange their
`received_lsn`. Each replica tracks the minimum `received_lsn`
across all peers (`min_peer_lsn`). Ops with LSN ≤ `min_peer_lsn`
are safe to GC — all replicas have them.

### Epoch and Writer Identity

Same as the Writer-based design:
- `data_loss_number` + `writer_epoch` (fenced on replicas)
- `config_version` (informational, not fenced)
- `session_token` for Writer authentication
- Coordinator manages epoch lifecycle
- Writer lifecycle protocol (graceful shutdown, crash fencing)

No changes to the epoch design.

### Committed LSN Propagation — The Lagging committed_lsn Problem

The Writer piggybacks `committed_lsn` on each `ReplicationItem` —
same as the Writer-based design. But now only W replicas receive it
directly. The remaining N-W learn `committed_lsn` during
peer-repair (the `PeerRepairResponse` includes `peer_committed_lsn`).

A replica takes the **max** of its own `committed_lsn` and any
`peer_committed_lsn` received during repair.

##### The Problem: Piggybacked committed_lsn Lags by One Write

`committed_lsn` is piggybacked on the **next** `ReplicationItem`
after the commit — not on the item that was committed. If the Writer
commits LSN=50 and then crashes before sending LSN=51, no replica
ever learns that LSN=50 was committed:

```
Writer commits LSN=50 (W=2 ACKed). committed_lsn=50 internally.
Writer crashes before sending LSN=51.
No replica ever receives ReplicationItem with committed_lsn=50.

All replicas think committed_lsn=49.

Coordinator detects Writer crash, bumps writer_epoch.
Coordinator pushes update_epoch to all replicas.
Replicas roll back uncommitted ops above committed_lsn:
  Replica₁: rolls back LSN=50 (above committed_lsn=49)  ← WRONG!
  Replica₂: rolls back LSN=50 (above committed_lsn=49)  ← WRONG!

LSN=50 was quorum-committed but rolled back by ALL replicas.
DATA LOSS of a committed op. ✗
```

This is a **correctness bug**, not a performance issue. The root
cause: `committed_lsn` is piggybacked on data items. If the Writer
commits and crashes before the next write, the committed value is
lost.

##### Does the Existing Leader-Based System Have This Bug?

**No** — because the primary doesn't roll back its own ops. After
failover, the new primary is selected by **highest LSN** (not by
`committed_lsn`). The new primary keeps everything it has.
Secondaries roll back, but only ops that weren't confirmed by the
new primary. The primary's local state is the source of truth.

In the Writer/peer-repair system, there is no primary — all
replicas roll back based on their local `committed_lsn`, which may
lag the Writer's authoritative value.

##### Fix: Coordinator Holds Authoritative committed_lsn

The Writer must report `committed_lsn` to the coordinator on every
commit (via the `WriterProgress` stream message). The coordinator
persists it in the CRD status. On epoch bump (Writer crash), the
coordinator pushes the authoritative `committed_lsn` to replicas
along with the new epoch.

```
Normal operation:
  Writer commits LSN=50 → sends WriterProgress { committed_lsn=50 }
  Coordinator stores committed_lsn=50 in CRD status

Writer crash:
  Coordinator has committed_lsn=50 (from last WriterProgress)
  Coordinator bumps writer_epoch
  Coordinator pushes to all replicas:
    update_epoch { new_epoch, authoritative_committed_lsn=50 }
  Replicas roll back uncommitted ops above 50 (not above their local 49)
  LSN=50 is preserved ✓
```

**The `update_epoch` RPC must include `committed_lsn` from the
coordinator.** Replicas use this value — not their local
`committed_lsn` — for rollback on epoch change.

```protobuf
message UpdateEpochRequest {
    Epoch new_epoch = 1;
    int64 authoritative_committed_lsn = 2;  // from coordinator, not local
}
```

##### Remaining Race: Writer Commits But Crashes Before Reporting

There is still a small window: the Writer commits LSN=50 (gets W
ACKs) but crashes before sending `WriterProgress { committed_lsn=50 }`
to the coordinator. The coordinator's last known `committed_lsn` is
49.

```
Writer: register LSN=50 → gets W ACKs → committed_lsn=50
Writer: crashes BEFORE sending WriterProgress
Coordinator: committed_lsn=49

Epoch bump: replicas roll back above 49
LSN=50 lost ← still a problem
```

**This window is small but real.** Mitigation options:

**Option A: Writer reports before returning to user (recommended).**
The Writer sends `WriterProgress` to the coordinator before
resolving the `replicate()` future to the user. This makes the
coordinator's `committed_lsn` always ≥ what the user has seen:

```rust
pub async fn replicate(&self, data: Bytes) -> Result<Lsn> {
    // ... send to W replicas, wait for W ACKs ...
    // Report to coordinator BEFORE returning to user
    self.coordinator.send(WriterProgress { committed_lsn: lsn }).await?;
    Ok(lsn)
}
```

**Latency impact depends on whether the send is fire-and-forget or
acknowledged:**

| Mode | Write latency | Remaining window |
|---|---|---|
| No report (current) | RTT(writer↔replica) | Full piggybacking lag |
| Fire-and-forget | RTT(writer↔replica) | TCP buffer flush (~μs) |
| Acknowledged | RTT(writer↔replica) + RTT(writer↔coordinator) | None |

**Fire-and-forget (recommended default):** The send is non-blocking
— a message pushed onto the already-open bidirectional gRPC stream.
No round-trip, no waiting for the coordinator to ACK. Write latency
is unchanged. The remaining window (TCP buffer not yet flushed when
Writer crashes) is microseconds — acceptable for most workloads.

**Acknowledged (opt-in for strict durability):** The Writer waits
for the coordinator to confirm it persisted `committed_lsn`. This
closes the window completely but adds one round-trip to the
coordinator on every write — potentially doubling latency if the
coordinator is in-cluster (~R + ~R = 2R). Available as a
configuration option for workloads that need absolute zero data loss,
similar to the `fsync` trade-off in databases.

**Coordinator must persist `committed_lsn` durably.** The
coordinator holds `committed_lsn` in memory from the Writer's
stream, but must also persist it to survive coordinator crashes.
The natural store is the **CRD status** (K8s API → etcd). If the
coordinator crashes and restarts with only in-memory state, a
subsequent Writer crash would leave the coordinator with
`committed_lsn=0` — causing total rollback of all ops.

However, writing to the K8s API on every commit is expensive
(~1-5ms per API call). The coordinator should **batch** CRD writes:

| Flush strategy | Coordinator crash window | CRD write load |
|---|---|---|
| Every commit | ~0 | ~1 API call per write (expensive) |
| Every 50-100ms | Up to flush interval | ~10-20/sec (manageable) |
| Every N commits | Up to N commits | Bounded |

**Batching re-opens a correctness window:** If both the coordinator
AND the Writer crash between CRD flushes, the persisted
`committed_lsn` is stale. This is a double-crash scenario
(coordinator + Writer) within a short window — rare but possible.

This is the same trade-off every database faces: PostgreSQL's
`synchronous_commit = on` (safe, slow) vs `off` (fast, small
window). **The recommendation is a configurable durability level:**

```
durability_level:
  relaxed:   # fire-and-forget + batched CRD flush (default)
             # Window: coordinator crash within flush interval
             # + Writer crash before next flush
  strict:    # acknowledged + per-commit CRD write
             # Window: none (fully durable)
             # Cost: +1-5ms per write (etcd round-trip)
```

**Option B: Coordinator queries replicas on epoch bump (NOT correct).**
Before rolling back, the coordinator asks all replicas for their
`received_lsn`. The `max(received_lsn)` across all replicas would
be used as `authoritative_committed_lsn`.

Cost: adds one round-trip to the epoch-bump path (coordinator →
all replicas → coordinator). This is the failover path, not the
write path, so latency is acceptable.

**However, Option B has a correctness issue.** If a replica crashes
between ACKing the Writer and the coordinator's query, the
coordinator cannot distinguish a committed op from an uncommitted
partial write:

```
N=3, W=2. Writer sends LSN=50 to Replica₁ and Replica₂.
Both ACK → committed. Writer crashes.
Replica₂ also crashes.

Coordinator queries survivors:
  Replica₁: received_lsn=50
  Replica₂: DEAD
  Replica₃: received_lsn=48

max(survivors) = 50. But was LSN=50 committed?
  - If both ACKed before Replica₂ died → YES (committed)
  - If Writer only sent to Replica₁ before crashing → NO (partial)

The coordinator sees identical state in both cases:
1 surviving replica with LSN=50. It CANNOT tell. ✗
```

The only source of truth for `committed_lsn` is the Writer itself
(which counted the ACKs) or a durable store the Writer reported to.
Replica `received_lsn` values alone are insufficient because they
don't prove quorum was met — they only prove the replica received
the op. **Option B is not recommended.**

**Option C: Accept the window.** Document that ops committed by
the Writer but not yet reported to the coordinator may be lost on
Writer crash. This is a very small window (microseconds between
quorum ACK and coordinator report) and may be acceptable for some
workloads.

##### Alternative Considered: Authority-Based Rollback (No committed_lsn)

Instead of tracking `committed_lsn`, an alternative is to pick an
**authority replica** on epoch bump and keep everything it has —
the same approach used by the existing leader-based system
(`driver.rs:340`: select new primary by highest LSN). Two variants
were considered:

**Option D: Max-LSN Authority (no pre-designation).**
On Writer crash, coordinator queries all replicas for
`received_lsn`, computes `authority_lsn = max(all survivors)`,
pushes `update_epoch { authority_lsn }`. Replicas keep everything
≤ `authority_lsn`. New Writer starts at `authority_lsn + 1`. No
`committed_lsn` tracking, no CRD writes, no fire-and-forget
window.

**Option E: Designated Preferred Replica.**
Coordinator designates one replica as "preferred." Writer always
includes it in W. On failover, preferred's `received_lsn` is the
authority. If preferred is also down, fall back to Option D.
Simpler failover but adds a soft role and a hotspot.

**Both options are rejected — they violate response consistency.**

The authority approach keeps ALL ops up to the max LSN, including
ops that never reached quorum. This conflates two semantically
distinct cases:

```
Scenario A — committed, then replica + Writer crash:
  Writer sends LSN=50 to R₁ and R₂ (W=2). Both ACK → committed.
  Client receives Ok(50).
  R₂ crashes, Writer crashes.
  Survivors: R₁ has LSN=50, R₃ does not.
  Authority says keep LSN=50 ← CORRECT (client saw success)

Scenario B — uncommitted, then Writer crash:
  Writer sends LSN=50 to R₁ only (W=2 needed, 1 ACK).
  Client receives Err(NoWriteQuorum).
  R₂ crashes (unrelated), Writer crashes.
  Survivors: R₁ has LSN=50, R₃ does not.
  Authority says keep LSN=50 ← WRONG (client was told write failed)
```

The coordinator sees **identical** replica state in both scenarios:
one survivor with LSN=50, one without. The authority approach keeps
LSN=50 in both cases. But in Scenario B, the client received a
**definitive error** (`Err(NoWriteQuorum)`) — the system explicitly
told the client the write did not succeed. Keeping the data after
recovery makes the system state **inconsistent with the client's
knowledge**.

**Why the existing leader-based system avoids this problem:**

In the existing leader-based kuberic (`PartitionDriver::failover`),
the new primary is elected only when the old primary has **crashed**.
The client's most recent interaction with the crashed primary is a
**transport error** (connection lost) — the client does not know
whether the in-flight operation succeeded or not. The operation is
**in-doubt**, not explicitly failed. Kuberic is free to keep or
discard in-doubt ops; the client has no expectation either way.

The peer-repair Writer system is different: the Writer can return
`Err(NoWriteQuorum)` to the client (a **definitive** response)
and then crash afterward. The client knows the write failed. The
system must honor this:

| Client status | Existing leader-based | Peer-repair Writer |
|---|---|---|
| In-doubt (transport error) | Primary crash caused the error — the only case | Writer crash before responding |
| Explicitly failed | N/A — primary stays alive after NoWriteQuorum | Writer returns Err, then crashes later |

In the "explicitly failed" case, **response consistency** requires
that the data is NOT present after recovery. The authority approach
cannot guarantee this because it lacks the information to distinguish
committed from uncommitted ops. Only the Writer (which counted ACKs)
or a durable store the Writer reported to can provide this.

**Conclusion:** `committed_lsn` tracking (Option A) is necessary for
correctness. The complexity it introduces (coordinator persistence,
fire-and-forget window, batching trade-off) is the cost of honoring
response consistency. The authority-based approach (Options D/E) is
simpler but sacrifices this property.

**Recommendation: Option A (fire-and-forget).** The coordinator
stream is already open, sending an extra progress message per commit
is negligible, and it closes the correctness gap to a microsecond
TCP buffer window. Option B is incorrect. Options D and E violate
response consistency. Option C is acceptable only for workloads that
tolerate rare data loss.

### Replica Building

Replica building uses **snapshot + peer-repair**:

1. **Snapshot (required for base data):** Existing replicas GC ops
   once all peers have confirmed receipt. A new replica joining with
   `received_lsn = -1` cannot repair from the beginning — those
   ops are long gone. It needs a full state snapshot first.

2. **Peer-repair (for the delta):** After the snapshot, the new
   replica has data through `snapshot_lsn`. The repair loop
   catches up the delta from `snapshot_lsn + 1` to the current head.

```
Operator                    Existing Replicas          New Replica
────────                    ─────────────────          ───────────
1. Create new pod
2. Pick snapshot source
   (highest committed_lsn)

3. Snapshot (copy protocol):
   Source replica ──────────────────────────────────►
     GetCopyState(up_to_lsn) ─────────────────────►  received_lsn = -1
     CopyStream(full state)  ─────────────────────►  Apply snapshot
                                                      received_lsn = snapshot_lsn

4. Peer-repair (delta):
   (repair loop starts automatically)
                              ◄── PeerRepairRequest ──
                              ── PeerRepairResponse ──►
                                 (ops from snapshot_lsn+1)

   ... repeat until caught up ...
                                                      received_lsn ≈ current

5. Add to config (tell Writer via coordinator)
```

**The copy protocol is the same as today's** `GetCopyState` /
`CopyStream` — any replica can serve as the snapshot source. The key
difference from the Writer-based design is that **the Writer is not
involved in the build at all**. The operator picks a source replica,
runs the copy, and the new replica's repair loop handles the
rest.

**When is snapshot needed vs. peer-repair-only?**

| Scenario | Mechanism |
|---|---|
| New replica (fresh pod) | Snapshot required, then peer-repair for delta |
| Restarted replica (has local state) | Peer-repair only — pulls missing ops from peers |
| Replica down briefly | Peer-repair only — peers still have the ops |
| Replica down longer than GC window | Snapshot required — peers have GC'd the gap |

**GC must account for builds:** While a build is in progress, existing
replicas must not GC below the snapshot source's `committed_lsn` at
snapshot time. The operator signals `BuildInProgress` to replicas (not
the Writer — replicas manage their own GC). After the new replica
catches up via peer-repair, `BuildComplete` resumes GC.

### Writer Crash Recovery

Writer crash is simpler than in the Writer-based design because the
Writer has **no ReplicationQueue**:

1. Writer crashes
2. Coordinator detects disconnect, bumps `writer_epoch`
3. Replicas roll back uncommitted ops (same as before)
4. New Writer connects, gets `committed_lsn` from coordinator
5. Sets `next_lsn = committed_lsn + 1`
6. Resumes writing

No queue to recover, no build to abort. Replicas already have all
committed ops (guaranteed by quorum). Peer-repair ensures any
lagging replica catches up from peers.

---

## Comparison: Writer-Based vs. Peer-Repair-Based

| Aspect | Writer-based | Peer-repair-based |
|---|---|---|
| **Write fan-out** | All N replicas | W replicas only |
| **Writer state** | `ReplicationQueue` (in-memory) | None (stateless after quorum ACK) |
| **Lagging replica catch-up** | Writer replays from queue | Replica pulls from peers |
| **Replica building** | Copy protocol + queue replay via Writer | Copy protocol (snapshot) + peer-repair (delta), no Writer involvement |
| **Writer crash impact** | Queue lost, build aborted | No impact (no queue) |
| **Write latency** | Same (wait for W ACKs) | Same (wait for W ACKs) |
| **Convergence latency** | Immediate (Writer sends to all) | Delayed (~100-300ms for N-W replicas) |
| **Replica complexity** | Simple (receive + ACK) | Higher (repair loop + op storage) |
| **Network traffic** | Writer → all N (write path) | Writer → W (write) + replica ↔ replica (peer-repair) |
| **Coordinator dependency** | High (build coordination) | Lower (no build coordination) |

### When to Choose Which

| Scenario | Recommended |
|---|---|
| All replicas must have data immediately | Writer-based (send to all) |
| Writer bandwidth is constrained | Peer-repair (send to W only) |
| Minimizing Writer complexity/state | Peer-repair (no queue) |
| Large datasets with frequent pod restarts | Peer-repair (self-healing) |
| Reads from any replica require latest data | Writer-based (all replicas up-to-date) |
| Reads tolerate ~100-300ms staleness | Peer-repair |

---

## Trade-Offs

### Advantages

1. **Stateless Writer** — no `ReplicationQueue`, no build
   coordination, no queue GC. Writer crash is trivial to recover
   from.
2. **Self-healing replicas** — lagging or new replicas catch up from
   peers automatically. No operator intervention for routine
   catch-up.
3. **Simpler replica building** — snapshot from any peer + automatic
   peer-repair for the delta. No Writer involvement, no queue
   replay coordination. The Writer doesn't even know a build is
   happening.
4. **Reduced Writer bandwidth** — sends to W instead of N.
5. **Eliminates KI-2 and KI-4** from Writer-based design (no queue
   to lose or grow unboundedly).

### Disadvantages

1. **Convergence delay** — N-W replicas are behind by one
   repair interval (~100-300ms). Reads from a lagging
   replica return stale data.
2. **Higher replica complexity** — each replica runs a repair
   loop, maintains an op store with GC, exchanges state with peers.
3. **More inter-replica traffic** — repair exchanges consume
   intra-cluster bandwidth. For N=3, 3 pairwise exchanges per
   interval.
4. **Op retention on replicas** — replicas must retain ops until all
   peers have them. If one replica is down for a long time, ops
   accumulate on the others (same problem as Writer's queue, but
   distributed).
5. **Snapshot still needed for new replicas** — peer-repair handles
   the delta, but a fresh replica needs a full snapshot first (same
   copy protocol as today). The simplification is that the Writer
   is uninvolved — the operator and replicas handle it directly.

---

## Open Questions

1. **Repair interval tuning:** 100ms gives fast catch-up but
   generates continuous background traffic. 1s reduces traffic but
   increases staleness. Should this be configurable per-partition?

2. **Op retention, GC, and rollback (resolved):** Two distinct
   operations on replica op stores:

   **GC (garbage collection):** Remove committed ops that all N
   replicas have confirmed receipt of. The data is safe — it's been
   applied to user state everywhere. This is routine cleanup to
   bound memory usage. GC condition: `lsn <= min(peer_received_lsn)`
   across all replicas.

   **Rollback (on epoch change):** Remove uncommitted ops that the
   Writer sent but never quorum-committed. These are partial/stale
   writes from a crashed Writer that should never have been applied.
   Rollback condition: `discard ops WHERE lsn > authoritative_committed_lsn`.
   Triggered by the coordinator on epoch bump via `update_epoch`.

   **Peer-repaired ops are never rolled back.** The committed-only rule
   guarantees that peer-repaired ops have `lsn <= committed_lsn`. Since
   `authoritative_committed_lsn >= committed_lsn`, the rollback
   rule never touches peer-repaired ops. No source tracking needed —
   the math works out naturally:
   - Peer-repaired ops: `lsn <= peer_committed_lsn <= authoritative` → preserved ✓
   - Writer-direct committed ops: `lsn <= authoritative` → preserved ✓
   - Writer-direct uncommitted ops: `lsn > authoritative` → rolled back ✓

3. **Snapshot trigger (resolved):** A replica needs a snapshot when
   the ops it's missing have been **GC'd from all peers**. This is
   a binary condition, not a threshold:

   During peer-repair, if a peer responds with ops starting at
   LSN X but the requester needs ops from LSN Y where Y < X (the
   peer has GC'd everything below X), peer-repair cannot fill
   the gap. The peer signals this:

   ```protobuf
   message PeerRepairResponse {
       int64 peer_committed_lsn = 1;
       int64 peer_oldest_available_lsn = 2;  // lowest LSN still in op store
       repeated ReplicationItem ops = 3;
   }
   ```

   If `peer_oldest_available_lsn > my_received_lsn + 1`, the gap is
   unrecoverable via peer-repair. The replica must request a
   snapshot from this peer (or another). After applying the snapshot,
   peer-repair resumes for the delta.

   **When this happens:**
   - Fresh replica (new pod): always needs snapshot
   - Restarted replica (has local state): only if down longer than
     the GC window (i.e., all peers advanced `min_peer_lsn` past
     this replica's `received_lsn` while it was down)
   - Briefly-down replica: peer-repair suffices (peers still have
     the ops)

4. **Read consistency:** Same as Writer-based design — reads from a
   lagging replica may return stale data. Should the peer-repair
   protocol include a "wait for LSN" mechanism?

5. **Committed LSN propagation lag:** Only W replicas learn
   `committed_lsn` directly. Others learn it during repair.
   For deferred-apply workloads (like SQLite), this delay means
   the N-W replicas defer application longer. Is this acceptable?

6. **Merkle tree for corruption detection:** For the common case
   (gap detection), LSN comparison is sufficient. Should replicas
   also maintain Merkle trees (like Riak AAE) for detecting silent
   corruption? This adds storage and CPU overhead but catches bit rot.

---

## Implementation Plan

### Phase 1: Peer-Repair Protocol

- Define `PeerRepairRequest` / `PeerRepairResponse` protobuf messages
- Implement `ReplicaPeerRepair` gRPC service on each replica
- Implement repair loop (periodic random peer selection)
- Implement op store with LSN-based GC

### Phase 2: Writer (Send to W)

- Reuse Writer from writer-based design, but remove `ReplicationQueue`
  and `send_to_all` — replace with `send_to_w` (pick W replicas)
- Coordinator integration (same protocol)

### Phase 3: Snapshot Fallback

- Reuse existing copy protocol (`GetCopyState` / `CopyStream`) for
  large-delta bootstrap
- Automatic snapshot trigger when repair delta exceeds
  threshold

### Phase 4: GC Coordination

- Implement distributed GC: track `min(peer_received_lsn)` across
  all replicas
- Ops below min are safe to GC
- Handle down replicas (GC must not advance past a down replica's
  last known LSN until operator removes it from config)

---

## Known Issues

Issues identified during design review (SoT premortem analysis) that
require further design work.

### KI-1: GC Watermark Disagreement (must-fix)

Replicas compute `min_peer_lsn` independently from pairwise
repair exchanges. Each replica has a potentially different
view of peers' progress, so two replicas may compute different
`min_peer_lsn` values and GC different ops.

```
Replica₁ repairs with Replica₂ → learns Replica₂ has LSN=50
Replica₁ repairs with Replica₃ → learns Replica₃ has LSN=45
Replica₁: min_peer_lsn = 45, GCs below 45

Replica₂ hasn't repaired with Replica₃ recently
Replica₂: min_peer_lsn = 40 (stale view of Replica₃)
Replica₂ GCs below 40

If Replica₃ actually needs LSN 41-44 from Replica₂, they're gone.
But Replica₁ still has them. So repair from Replica₁ works.
```

**The issue is not data loss** (as long as ANY replica retains the
ops), but it complicates the snapshot trigger — a replica may need
to try multiple peers to find one that hasn't GC'd the needed range.

**Potential fix:** Coordinator-managed GC watermark. The coordinator
collects `received_lsn` from all replicas (via repair status
reports or direct queries) and publishes a global
`safe_gc_lsn = min(all replicas' received_lsn)`. Replicas GC only
below this coordinator-published value.

### KI-2: Inter-Replica Partition Detection (must-fix)

The design assumes all replicas can reach each other for
peer-repair. If two replicas are partitioned from each other
(but both reachable by the Writer and coordinator), peer-repair
between them stalls silently. Effects:

- GC is blocked: the partitioned replica's `received_lsn` doesn't
  advance from the other's perspective, blocking GC.
- Data divergence: the partitioned replica falls behind and can
  only catch up from the Writer's W-subset.

**Potential fix:** Coordinator pushes replica health/reachability
status. Replicas skip unreachable peers in GC watermark calculation.
The coordinator can also detect partitions and trigger snapshot
rebuilds for replicas that fall too far behind.

### KI-3: Rollback of Already-Applied Ops (must-fix)

Once an op is applied to user state via `StateProvider` and
`acknowledge()`d, it cannot be un-applied by the replication
system. If an epoch change triggers rollback of an op that was
already applied (because the replica received it directly from the
Writer and applied it before learning it was uncommitted), the user
state diverges from the op store.

**This is the same problem in the existing leader-based system** —
today's secondaries apply ops via `OperationStream` before quorum
is confirmed. The existing system handles it by choosing a new
primary with the highest LSN (so applied ops are preserved).

**In the peer-repair system:** The coordinator must ensure that
`authoritative_committed_lsn` (used for rollback) is never below
any op that has been applied and acknowledged by any replica.
Option A (Writer reports `committed_lsn` before returning to user)
ensures this — if the user hasn't seen the LSN, no replica has
applied it yet.

### KI-4: Quiescent Writer Stalls committed_lsn (should-fix)

If the Writer stops writing (no new ops), `committed_lsn` is never
piggybacked on new items. Replicas that only received ops via
peer-repair learn `committed_lsn` from peers, which also stalls.

**Fix:** Writer sends periodic heartbeat to W replicas with just
`committed_lsn` (no data). Or: the coordinator pushes
`committed_lsn` to all replicas periodically. Not a correctness
issue (peer-repair still works, just slower convergence) but
affects deferred-apply workloads.

### KI-5: W-Subset Selection Strategy (should-fix)

If the Writer always picks the same W replicas (e.g., lowest-latency),
some replicas never receive direct writes. Effects:

- Those replicas are always one repair interval behind
- They become GC bottlenecks (their `received_lsn` advances slower)
- Write load is unbalanced across replicas

**Fix:** Round-robin or random W-selection distributes writes evenly.
The Writer should rotate which replicas are in each write's W-subset.

### KI-6: New Replica Repair Loop Before Snapshot (should-fix)

A new replica starts its repair loop immediately on creation,
before the operator initiates the snapshot. Early repair
attempts fail (peers respond with `peer_oldest_available_lsn` > 0,
gap unrecoverable). Harmless but wastes network and log noise.

**Fix:** New replica starts repair loop only after snapshot
completes (operator signals readiness via config update).

---

## References

- Demers, A. et al. (1987). *Epidemic Algorithms for Replicated
  Database Maintenance.* PODC '87.
- Riak KV. *Active Anti-Entropy.*
  [docs.riak.com](https://docs.riak.com/riak/kv/latest/learn/concepts/active-anti-entropy/index.html)
- Cassandra. *Dynamo Architecture — Replica Synchronization.*
  [cassandra.apache.org](https://cassandra.apache.org/doc/latest/cassandra/architecture/dynamo.html)
- Writer-based replication design:
  [writer-based-replication.md](writer-based-replication.md)
- Leaderless quorum survey:
  [leaderless-quorum-replication.md](../../background/leaderless-quorum-replication.md)
