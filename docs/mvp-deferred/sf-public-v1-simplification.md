# SF Public V1 Simplification: deferred MVP work

- Work ID: sf-public-v1-simplification
- Core outcome: Decide whether the dormant Public V1 implementation needs behavior-preserving simplification before Phase 6.

## Decisions

### PVS-01: Consolidate controller session-renewal acceptance wrappers
- Status: DEFERRED
- Reason: Five thin wrappers exist, but they are named feature-gated test-bridge integration seams. Consolidation currently offers only a minor syntactic reduction without a demonstrated maintenance benefit.
- MVP alternative: Retain the explicit transitions.
- Revisit when: Phase 6 creates concrete call-site duplication or a measurable reduction beyond line count.
- Evidence: `kuberic-controller/src/evaluator/public_v1_session_renewal.rs` defines the acceptance wrappers, and `kuberic-controller/src/evaluator/test_bridge.rs` exposes them to cross-crate tests.

### PVS-02: Flatten PostgreSQL Public V1 dispatch into the legacy implementation
- Status: IMPLEMENTED
- Reason: The service no longer selects a legacy/Public V1 mode. PostgreSQL remains an independent custom Replicator on the ordinary Public V1 lifecycle.
- MVP alternative: None.
- Revisit when: Only for behavior-preserving custom-Replicator simplification with measured value.
- Evidence: `examples/postgres/src/service.rs` always constructs the Public V1 settings path.

### PVS-03: Merge Public V1 report authority derivation with callback-program selection
- Status: DEFERRED
- Reason: Both paths inspect the operation program, but one produces execution instructions and the other produces persisted completion evidence. A shared abstraction could move complexity without reducing it.
- MVP alternative: Preserve the exhaustive independent mappings and compiler coverage.
- Revisit when: Characterization proves one canonical descriptor can represent both mappings without weakening corrupt-state failures or wire evidence.
- Evidence: `kuberic-runtime/src/host/public_lifecycle_program.rs` selects execution instructions, while `kuberic-runtime/src/host/public_lifecycle.rs` derives completion evidence.

### PVS-04: Change schema-11 AgentState JSON layout or split Public V1 records into dedicated tables
- Status: IMPLEMENTED
- Reason: The replacement uses one schema-12 `agent_state` layout and removed the migration API, migration ledger, and managed continuation tables.
- MVP alternative: None.
- Revisit when: A future fresh-state layout measurably reduces complexity.
- Evidence: `kuberic-runtime/src/host/state.rs` and `kuberic-runtime/src/host/sqlite_store.rs`.
- Decision update: On 2026-10-12, the user stated that the design is not fixed and compatibility is not required.

### PVS-05: Remove apparently test-only Public V1 controller or runtime surfaces
- Status: IMPLEMENTED
- Reason: Obsolete preview/legacy controller modules, endpoints, selectors, and production branches were removed. Retained helpers are explicitly Public V1-named testing harnesses and are not production-selected.
- MVP alternative: None.
- Revisit when: A retained testing helper loses a concrete defect-sensitive consumer.
- Evidence: `kuberic-controller/src/evaluator/testing.rs`, `kuberic-controller/tests/public_api.rs`, and the KSF controller-module inventory.
