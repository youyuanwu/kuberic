# Topology-aware placement and primary balancing

This V2 controller feature supersedes the removed V1 operator work from
`youyuanwu/kuberic#90`, archived in Joyjeet045/kuberic at
`archive/pr-90-topology-placement-v1`. The V2 implementation keeps scheduling
and primary movement separate: Kubernetes places Pods, while optional primary
balancing submits an ordinary planned switchover request to the existing
level-triggered evaluator.

## Replica placement

Omitting `spec.placement` uses Preferred mode on `kubernetes.io/hostname`.
New replica Pods receive a preferred self-set anti-affinity rule with weight
100. The selector uses only `operator.kuberic.io/set-uid`, so other
`KubericSet`s and other namespaces never compete with the set being scheduled.

```yaml
apiVersion: operator.kuberic.io/v1alpha1
kind: KubericSet
metadata:
  name: orders
spec:
  replicas: 3
  image: localhost/kvstore2:level-triggered-v1
  placement:
    mode: Required
    topologyKey: kubernetes.io/hostname
    nodeSelector:
      pool: storage
    tolerations:
      - key: dedicated
        operator: Equal
        value: storage
        effect: NoSchedule
```

`mode` is `Preferred` or `Required`; the default is `Preferred`. Required mode
adds hard self-set anti-affinity and a required node-affinity `Exists`
expression for the configured `topologyKey`. Nodes without that label are
ineligible for newly created Pods. `nodeSelector` and compact tolerations are
passed through to the PodSpec. Existing Pods are never evicted or recreated
only because placement policy changed; edits apply to subsequently created
replica Pods.

The CRD intentionally exposes only compact placement fields. Native Kubernetes
`Affinity`, `Toleration`, and `TopologySpreadConstraint` schemas are not embedded
because the controller CRD has a strict size guard.

## Placement diagnostics

The controller projects placement diagnostics into status conditions without
using evaluator-owned condition types:

- `ReplicaPodUnschedulable=True` when a replica Pod reports
  `PodScheduled=False` with reason `Unschedulable`.
- `RequiredTopologyViolation=True` for Required-mode live bound Pods when a
  node lacks the topology label or more than one live Pod from the set is in
  the same topology domain.
- `RequiredTopologyUnverified=Unknown` when a bound Pod's node is missing from
  the observed Node inventory, or Node inventory itself was unavailable.

These conditions report the current observation only. They do not relax Required
placement, promote Pending replicas, or authorize eviction.

## Automatic primary balancing

Primary balancing is opt-in:

```yaml
apiVersion: operator.kuberic.io/v1alpha1
kind: KubericSet
metadata:
  name: balanced-orders
spec:
  replicas: 3
  image: localhost/kvstore2:level-triggered-v1
  placement:
    mode: Preferred
    topologyKey: topology.kubernetes.io/zone
  primaryBalancing:
    mode: Automatic
    topologyKey: topology.kubernetes.io/zone
    cooldownSeconds: 300
    minimumImprovement: 1
```

The planner only runs when the evaluator already reports a stable, unchanged
status: the set is initialized, has no active transition/provisioning/cleanup,
has no pending unfinished `spec.switchover`, has no primary/quorum failure, and
every committed member reports healthy stable authority. The accepted primary
must still have write access. The balancer never changes runtime protocol state
directly; it patches:

```yaml
spec:
  switchover:
    requestId: auto-balance-<uid-prefix>-<epoch>-<unix-seconds>
    targetReplicaId: <committed-secondary-id>
```

The existing planned-switchover evaluator admits, executes, rejects, restores,
or compensates that request with the same safety gates as a user request.

Primary counts are computed across all observed `KubericSet` statuses using the
accepted primary's Pod `nodeName` and the selected Node topology label. Unknown
primary placement evidence suppresses automatic balancing rather than treating
unknown load as zero. Candidate secondaries must already be committed members in
a different, less-loaded domain. For source count `s` and target-domain count
`t`, a move is useful only when `s - t - 1 >= minimumImprovement`; with the
default threshold, `2 -> 0` moves and `1 -> 0` does not. Ties are deterministic:
target domain count, target node count, then replica ID.

## Deferred from the V1 PR

- V1 failover-time density ordering is intentionally not ported. V2 election
  and recovery are proof-backed in the evaluator, and topology density should
  not affect failover correctness until a separately verified design exists.
- The compact V2 CRD does not embed arbitrary native affinity or topology-spread
  schemas. Only `nodeSelector` and compact tolerations are exposed.
- Stabilization windows, Events, metrics, and cross-controller reservation
  coordination remain deferred. The supplied controller deployment still uses
  one active controller replica.
- Cooldown is enforced for auto-generated balance receipts, whose request IDs
  carry the planner timestamp. Current V2 switchover receipts do not include a
  timestamp for arbitrary user-authored request IDs.
