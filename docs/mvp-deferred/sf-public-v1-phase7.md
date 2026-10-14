# SF Public V1 Replacement: deferred MVP work

- Work ID: sf-public-v1-phase7
- Core outcome: Replace the legacy built-in stack with the sole fresh-state Public V1 path and validate it through existing repository workflows.

## Decisions

### P7D-01: Image publication and production cutover
- Status: DEFERRED
- Reason: Phase 7 validates source, archives, manifests, and owned-cluster workflows. Publishing images or activating production traffic is a separate operational product decision.
- MVP alternative: Build the ordinary controller/provider images and manifests through existing repository paths, apply them only in the disposable owned verification cluster, and avoid image publication, production mutation, destructive storage action or traffic enablement.
- Revisit when: A separately scoped production-distribution and fresh-deployment project is approved.
- Evidence: `README.md` and `docs/features/kuberic/stateless-default-replicator.md`.

### P7D-02: Old-state migration and rolling compatibility
- Status: REJECTED
- Reason: The replacement-only release is validated from fresh state and intentionally rejects old controller and agent state plus every supported in-repository provider format rather than interpreting or enrolling it. Independent custom Replicators own classification and rejection of their provider storage because the protected public interfaces expose no generic storage-format introspection.
- MVP alternative: Initialize only the replacement schema, fail closed on old controller/agent and supported-provider state, and require each independent custom Replicator to reject its own prior provider storage before participation.
- Revisit when: Only through a separately approved product decision that changes the no-compatibility design.
- Evidence: `docs/features/kuberic/stateless-default-replicator.md` defines fresh-storage activation, provider-owned application durability and no generic migration contract; `docs/features/kuberic/service-fabric-alignment.md` excludes preservation and mixed-version operation.

### P7D-03: Public V1 transport authentication or peer authorization
- Status: REJECTED
- Reason: Public V1 intentionally has no transport credential contract. Replica identity, session, epoch, history, storage, build and operation values are correctness fences rather than credentials.
- MVP alternative: Preserve exact lifecycle fencing and use the controller command credential, resource identity and Kubernetes RBAC only as separate control-plane fences.
- Revisit when: Only if the credential-free transport design is explicitly replaced.
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
- Revisit when: A selected consumer requires the V2 transactional contract after the public-only V1 replacement is stable.
- Evidence: `docs/features/kuberic/service-fabric-alignment.md` places optional V2 after the Phase 7 public-only V1 replacement.

### P7D-06: Dedicated reproducible-build or attestation infrastructure
- Status: DEFERRED
- Reason: Phase 7A needs reviewable source, build and runtime test results, but it does not publish images or promise an external supply-chain attestation contract.
- MVP alternative: Use the existing checked-out revision, Cargo/nextest archive, Docker image build, CI results and owned KinD verification without adding a separate release builder or evidence format.
- Revisit when: Phase 7B publication or a separately approved provenance, SBOM or external-attestation requirement needs independent verification outside repository CI.
- Evidence: `README.md` documents source/local-CI distribution rather than published images; existing Cargo, nextest, Docker, Kustomize, CI, and owned-KinD paths remain the only verification machinery.

### P7D-07: Generic host-side custom-provider storage introspection
- Status: DEFERRED
- Reason: Provider formats and durable application state are provider-owned, and the protected Public V1 interfaces expose no generic storage-format classification contract.
- MVP alternative: Reject old replacement-agent and supported in-repository provider state at their existing boundaries; require each independent custom Replicator to classify and reject its own prior provider storage before participation.
- Revisit when: A separately approved public provider-storage classification contract can be added without violating protected interface stability.
- Evidence: `docs/features/kuberic/stateless-default-replicator.md` assigns application bytes and progress to the provider; the project requirement forbids changes to the protected Replicator and StateProvider method sets.

### P7D-08: Full Phase 6 behavioral matrix inside KinD
- Status: DEFERRED
- Reason: Repeating every mature source/provider test inside a cluster would duplicate coverage and materially increase runtime without proving a distinct deployment-specific failure for each case.
- MVP alternative: Run the complete replacement boundary through existing nextest/provider tiers and retain bootstrap, replacement, and failover as the owned KinD deployment scenarios.
- Revisit when: A demonstrated defect shows that an additional material-bound behavior has a distinct image or deployment failure mode not covered by the owned bootstrap/replacement/failover journey.
- Evidence: `docs/features/kuberic/testing.md` separates source/provider, PostgreSQL and live-cluster tiers and documents the existing Cargo, nextest, provider, Docker, Kustomize, CI, and KinD verification paths.

### P7D-09: Broad mutation-testing framework for replacement eligibility
- Status: DEFERRED
- Reason: KSF-16 needs proof that its concrete eligibility oracle detects semantic reintroduction, not an exhaustive mutation-testing campaign or new harness.
- MVP alternative: Maintain exact source, artifact and runtime-registration allowlists and one differently named extra-owner or endpoint mutation fixture that must fail eligibility.
- Revisit when: A regression escapes the allowlist model or the replacement capability surface expands enough that one representative mutation no longer validates the oracle.
- Evidence: `docs/features/kuberic/service-fabric-alignment.md` defines KSF-16 as the legacy-continuation guard; the existing project test strategy favors bounded, risk-specific checks over a new general framework.

### P7D-10: Kubernetes cutover Lease
- Status: REJECTED
- Reason: The replacement-only artifact does not implement migration, compatibility or production cutover. A prebuilt Lease and `coordination.k8s.io` RBAC add control-plane machinery that does not validate the Phase 7A outcome.
- MVP alternative: Confine the controller through one installation namespace and namespace-scoped Role/RoleBinding permissions; keep any future production activation procedure outside this artifact.
- Revisit when: Only if a separately approved production-activation design demonstrates that a Kubernetes Lease is necessary rather than relying on offline operational sequencing.
- Evidence: User decision on 2026-10-14: “Why there are leases? We don't care about compatibility and migrationa”; Phase 7A specification and implementation plan contain no Lease requirement.

### P7D-11: Schema migration API and migration ledger
- Status: REJECTED
- Reason: The replacement accepts only fresh schema-12 state and rejects every earlier or partial schema. A no-op migration API and `schema_migrations` ledger add compatibility machinery with no production caller.
- MVP alternative: Keep one exact create/reopen path plus immutable old-state rejection tests that prove bytes and provider files remain unchanged.
- Revisit when: Only if a separately approved requirement introduces an actual supported schema migration.
- Evidence: User selected simplification candidate P7-SIM-01 on 2026-10-14; current plan explicitly excludes migration and compatibility.

### P7D-12: Duplicate cutover-oriented KSF inventories
- Status: REJECTED
- Reason: Separate future-cutover obligations and duplicate synthetic inventories obscure the ordinary replacement ownership contract.
- MVP alternative: Maintain one canonical KSF-09/10/16 ownership inventory and one differently named extra-owner/endpoint fixture that exercises the same eligibility oracle.
- Revisit when: The replacement capability surface materially expands and needs a new distinct ownership dimension.
- Evidence: User selected simplification candidate P7-SIM-02 on 2026-10-14; KSF-16 remains required as a replacement correctness guard.

### P7D-13: Transitional managed/preview adapters
- Status: REJECTED
- Reason: Adapters, dual selectors and compatibility bridges would preserve paths whose removal is the Phase 7A outcome and create non-final intermediate behavior.
- MVP alternative: Retain the verified partial implementation and finish the direct one-path deletion of managed, preview and old-protocol branches.
- Revisit when: None for this replacement; rollback is a pre-merge source revert rather than retained runtime compatibility.
- Evidence: User selected P7-SIM-03 and chose to retain/revise the current implementation on 2026-10-14.

### P7D-14: Operational migration/cutover/release documentation
- Status: REJECTED
- Reason: Detailed Lease, digest-bundle, namespace-quarantine, export/reimport and activation procedures describe machinery that this project phase does not support or intend to implement.
- MVP alternative: Document fresh-state replacement, explicit old-state rejection, ordinary repository verification and a concise statement that any production activation is separately approved.
- Revisit when: A concrete production-activation project is separately scoped and approved.
- Evidence: User selected simplification candidate P7-SIM-04 on 2026-10-14 and explicitly requested an audit of all migration and compatibility content.
