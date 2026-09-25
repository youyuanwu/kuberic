# kuberic-controller

Kubernetes Failover Manager-equivalent for the independent level-triggered
Kuberic stack.

The controller watches `operator.kuberic.io/v1alpha1` `KubericSet` resources
and their owned Pods, PVCs, Services, and Secrets. Each reconcile observes and
normalizes the complete state, calls the pure evaluator in
`kuberic-protocol`, applies independent resource convergence, and dispatches
at most one fenced authority-changing command. Stable, waiting, and unsafe
states all use bounded re-observation.

The controller owns configuration selection, failure timing, routing fences,
replacement provisioning, status projection, and optimistic-concurrency
acceptance. It does not own replica-local effect sequencing, replication
quorum credit, application durability, or a workflow phase journal.

Planned switchover uses `spec.switchover.requestId` and
`spec.switchover.targetReplicaId` (a committed logical secondary ID). Acceptance
freezes exact identities in `status.transition.switchover`; the active request
cannot be cancelled or retargeted. `status.lastSwitchover` retains the latest
terminal receipt; admission rejections use `SwitchoverRejected` conditions.
Authority acceptance precedes write grant and exact-Pod routing publication.
Restoration, newer-epoch compensation, and Unsafe closure remain evidence-gated.

Lowering `spec.replicas` enables secondary-only scale-down: desired count is
target and minimum (floor one), with one frozen highest-ID committed secondary
removed at a time. Previous-read/reduced-write evidence precedes atomic reduced
topology/policy and `secondaryScaleDownCleanup` acceptance. Separate local commit,
write grant, and routing can restore service while cleanup is pending.
Exact-name GETs and frozen UID/fresh resource-version deletes enforce
endpoint→Pod→PVC cleanup. An unreachable target is fenced only after commit
by exact Pod deletion; PVC deletion waits for observed Pod absence and is permanent.
`lastSecondaryRemoval` retains convergence proof, never deletion authority.

Replacement freezes `pendingReplacementCleanup` before provisioning and moves
it unchanged to `lastReplacement` at commit. Both replacement and scale-down
serialize subsequent operations until exact cleanup completes; label loss,
finalizers, and ambiguous replies cannot overwrite the obligation or authorize
deletion of same-name replacement UIDs.

Protocol 6 and agent store schema 2 require a fresh coordinated deployment.
Scale-up, primary/explicit-target removal, and active-removal cancellation are
unsupported. There is no maximum write-interruption guarantee.

Deployment assets are under `deploy/` and are intentionally isolated from the
classic `kuberic.io/v1` operator. The controller and sample images are
development/CI artifacts and are not currently published release targets.

See the [level-triggered operator guide](../docs/features/kuberic/level-triggered-operator.md)
for the CRD, deployment, supported operations, diagnostics, and limitations.
The guide includes the [request example and retry contract](../docs/features/kuberic/level-triggered-operator.md#planned-switchover).
See also [secondary scale-down usage and conditions](../docs/features/kuberic/level-triggered-operator.md#secondary-scale-down).
