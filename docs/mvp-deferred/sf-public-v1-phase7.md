# SF Public V1 Replacement Release: deferred MVP work

- Work ID: sf-public-v1-phase7
- Core outcome: Produce and validate one immutable replacement-only Public V1 release bundle from fresh state without activating it in production.

## Decisions

### P7D-01: Image publication and production cutover
- Status: DEFERRED
- Reason: Phase 7A proves the exact replacement-only artifact and its unapplied manifests. Publishing and activating that artifact are irreversible operational actions requiring separate approval and fresh deployment state.
- MVP alternative: Produce digest-addressed OCI layouts, a digest-pinned deployment overlay and verification evidence without publishing or applying them.
- Revisit when: A separate explicit irreversible-cutover approval confirms required application exports completed or no old data is retained, and fresh controller, agent, application, namespace and resource state is ready.
- Evidence: `docs/features/kuberic/stateless-default-replicator.md` separates replacement artifact finalization from the offline atomic cutover; `.paw/work/sf-public-v1-phase7/Spec.md` excludes every Phase 7B activation action.

### P7D-02: Old-state migration and rolling compatibility
- Status: DEFERRED
- Reason: The replacement-only release is validated from fresh state and intentionally rejects every legacy and preview store rather than interpreting or enrolling it.
- MVP alternative: Initialize only the replacement schema and fail closed on old controller, agent and application state.
- Revisit when: A production adoption requirement explicitly requires retained data, backup restoration, enrollment, rolling upgrades or mixed-version operation.
- Evidence: `docs/features/kuberic/stateless-default-replicator.md` defines fresh-storage activation and excludes preservation or rolling migration; Phase 7A acceptance requires rejection of schemas 6, 7 and 11.

### P7D-03: Public V1 transport authentication or peer authorization
- Status: DEFERRED
- Reason: Public V1 intentionally has no transport credential contract. Replica identity, session, epoch, history, storage, build and operation values are correctness fences rather than credentials.
- MVP alternative: Preserve exact lifecycle fencing and use the controller command credential, cutover Lease, resource identity and Kubernetes RBAC only as separate control-plane cutover fences.
- Revisit when: A separately approved security requirement defines TLS identity, workload identity, tokens or another replication-transport credential model.
- Evidence: `docs/features/kuberic/replicator-boundary.md` and `docs/features/kuberic/stateless-default-replicator.md` document the unauthenticated Public V1 boundary.

### P7D-04: Transparent completion of crash-interrupted client requests
- Status: DEFERRED
- Reason: Replacement validation requires provider durability before acknowledgement and conservative ambiguity after process loss, not a Replicator-owned write-recovery journal.
- MVP alternative: Return failure or disconnection for ambiguous completion and rely on application operation identities for safe retry.
- Revisit when: A separately reviewed client contract requires transparent continuation across host-process failure.
- Evidence: `docs/features/kuberic/stateless-default-replicator.md` excludes transparent crash continuation and requires conservative ambiguous-write handling.

### P7D-05: Optional V2 transactional durability
- Status: DEFERRED
- Reason: Provider-owned Public V1 application durability is sufficient for the replacement-only release and its fresh-state acceptance boundary.
- MVP alternative: Keep state providers authoritative for application bytes, progress, retained history and copy state.
- Revisit when: A selected consumer requires the V2 transactional contract after the public-only V1 replacement is stable and activated.
- Evidence: `docs/features/kuberic/service-fabric-alignment.md` places optional V2 after the Phase 7 public-only V1 replacement.
