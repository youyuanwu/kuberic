# Phase 4 Remove-Replica Safety Traceability

Deletion gate status: **passed on 2026-09-07**. Every SC-002 invariant maps to
retained framework-native, shared runner/kernel, Kubernetes provider, or
live-cluster coverage. No row relies only on the legacy remove pilot.

| # | SC-002 invariant | Retained passing replacement test(s) | Coverage |
|---:|---|---|---|
| 1 | Immutable operation mode | `remove_replica_execution_admission_is_structured_compact_and_derives_reduced_topology`; `scale_down_freeze_rejects_target_generation_drift_after_preadmission` | Native workflow/domain |
| 2 | Pre-commit restart | `test_framework_native_remove_replica_every_boundary_restart` | Native reconciler |
| 3 | Post-commit restart | `test_framework_native_remove_replica_every_boundary_restart`; `test_framework_native_remove_replica_terminal_precedes_status_publication` | Native reconciler |
| 4 | Exact prepared command | `remove_replica_execution_reuses_the_exact_prepared_command`; `framework_native_remove_replica_fr019_exact_effect_dispatch` | Native workflow/adapter |
| 5 | Direct-dispatch authority | `remove_replica_execution_boundaries_are_tagged_and_never_store_mutable_operation_state`; `framework_native_remove_replica_fr019_exact_effect_dispatch` | Native workflow/adapter |
| 6 | One-use dispatch authority | `freshly_accepted_terminal_reloads_before_publication_and_permit_is_one_use` | Shared runner |
| 7 | Lost effect reply | `test_framework_native_remove_replica_lost_reply_and_quarantine` | Native reconciler |
| 8 | Uncertain persistence write | `test_framework_native_remove_replica_conflict_and_unknown_write_reload`; `conflict_unknown_write_and_persistence_failure_stop_before_redelivery` | Native reconciler/shared runner |
| 9 | Conflict reload | `test_framework_native_remove_replica_conflict_and_unknown_write_reload`; `host_rejects_unsupported_format_and_reloads_after_unit_conflict` | Native reconciler/shared provider |
| 10 | Authoritative observation recovery | `remove_replica_execution_replays_evidence_deterministically`; `framework_native_remove_replica_fr019_observation_collection` | Native workflow/adapter |
| 11 | Exact primary-status gap without churn | `test_framework_native_remove_replica_exact_primary_status_gap_has_no_churn` | Native reconciler |
| 12 | Exact target-status gap without churn | `test_framework_native_remove_replica_exact_target_status_gap_has_no_churn` | Native reconciler |
| 13 | Commit evidence | `remove_replica_execution_transition_validation_is_monotonic`; `framework_native_remove_replica_fr019_terminal_validation` | Native workflow/adapter |
| 14 | Configuration authority | `remove_replica_execution_validates_completed_terminal_against_immutable_admission`; `framework_native_remove_replica_fr019_authority_and_preparation` | Native workflow/adapter |
| 15 | Correlated primary role evidence | `remove_replica_execution_captures_exact_retained_terminal_evidence`; `framework_native_remove_replica_fr019_observation_collection` | Native workflow/adapter |
| 16 | Correlated lifecycle evidence | `remove_replica_execution_captures_exact_retained_terminal_evidence`; `framework_native_remove_replica_fr019_observation_collection` | Native workflow/adapter |
| 17 | UID-fenced label cleanup | `framework_native_remove_replica_uid_fenced_label_and_delete_commands`; `test_framework_native_remove_replica_uid_fences_replacement` | Native adapter/reconciler |
| 18 | UID-fenced deletion | `framework_native_remove_replica_uid_fenced_label_and_delete_commands`; `test_framework_native_remove_replica_uid_fences_replacement` | Native adapter/reconciler |
| 19 | Post-commit connection cleanup | `missing_exact_primary_status_never_proves_connection_absence`; `framework_native_remove_replica_fr019_terminal_validation` | Retained domain/native adapter |
| 20 | Incarnation fencing | `remove_replica_execution_reuses_the_exact_prepared_command`; `framework_native_remove_replica_fr019_authority_and_preparation` | Native workflow/adapter |
| 21 | Epoch fencing | `remove_replica_execution_reuses_the_exact_prepared_command`; `framework_native_remove_replica_fr019_authority_and_preparation` | Native workflow/adapter |
| 22 | Bounded redrive of at most three attempts | `exhausted_known_catch_up_state_is_failed_precommit_incomplete`; `remove_replica_execution_maximum_fault_history_and_terminal_fit_independent_bounds` | Retained domain/native admission |
| 23 | Corrupt record handling | `remove_replica_execution_distinguishes_incompatible_and_malformed_contracts`; `load_distinguishes_absence_success_and_malformed_objects` | Native workflow/Kubernetes provider |
| 24 | Incompatible record handling | `remove_replica_execution_distinguishes_incompatible_and_malformed_contracts`; `checkpoint_dispositions_cover_rejected_and_incompatible` | Native workflow/shared runner |
| 25 | Distinct unsafe terminal handling | `remove_replica_execution_nested_terminal_denies_unknown_fields`; `restart_and_incomplete_states_use_distinct_typed_dispositions` | Native workflow/retained domain |
| 26 | Distinct inexact terminal handling | `remove_replica_execution_validates_completed_terminal_against_immutable_admission`; `framework_native_remove_replica_fr019_terminal_validation` | Native workflow/adapter |
| 27 | Terminal-before-status ordering | `test_framework_native_remove_replica_terminal_precedes_status_publication`; `freshly_accepted_terminal_reloads_before_publication_and_permit_is_one_use` | Native reconciler/shared runner |
| 28 | Redelivery without duplicated uncertain effects | `test_framework_native_remove_replica_lost_reply_and_quarantine`; `conflict_unknown_write_and_persistence_failure_stop_before_redelivery` | Native reconciler/shared runner |
| 29 | Three-member admission | `remove_replica_execution_admission_is_structured_compact_and_derives_reduced_topology` | Native admission |
| 30 | Active admission | `remove_replica_execution_rejects_all_six_one_byte_over_bounds`; `remove_replica_execution_maximum_fault_history_and_terminal_fit_independent_bounds` | Native admission/kernel |
| 31 | Terminal admission | `remove_replica_execution_rejects_all_six_one_byte_over_bounds`; `remove_replica_execution_maximum_fault_history_and_terminal_fit_independent_bounds` | Native admission/kernel |
| 32 | Persisted-record owner identity | `framework_native_remove_replica_checkpoint_owner_is_exact_and_non_controlling`; `owner_reference_is_validated_locally_and_preserved_across_writes` | Native construction/Kubernetes provider |
| 33 | Owner garbage collection | `validates_real_api_cas_watch_compaction_and_ambiguous_recovery` (`KUBERNETES_CHECKPOINT_LIFECYCLE owner_gc=passed`) | Isolated live Kubernetes API |
| 34 | Retained-record cleanup authorization | `checkpoint_rbac_examples_are_structural_and_lifecycle_specific` | Shared RBAC authorization |

## FR-017 Outcome Matrix

`framework_native_remove_replica_fr017_outcome_matrix_is_complete` and
`durable_runner_fr017_common_outcome_matrix_is_complete` cover active,
terminal, incompatible, rejected, isolated, conflict reload, unknown-write
reload, persistence failure, and nondeterminism. All nine outcomes are
reachable; none is declared contract-impossible. Switchover retains its
operation-specific matrix in
`test_durable_execution_switchover_pilot_fr017_operation_outcome_matrix`.

## FR-019 Responsibility Tests

The six independently named native remove tests are:

1. `framework_native_remove_replica_fr019_observation_collection`
2. `framework_native_remove_replica_fr019_authority_and_preparation`
3. `framework_native_remove_replica_fr019_exact_effect_dispatch`
4. `framework_native_remove_replica_fr019_deadline_policy`
5. `framework_native_remove_replica_fr019_terminal_validation`
6. `framework_native_remove_replica_fr019_publication_handoff`

## Phase 4 Verification Record

- Native adapter/FR-017/FR-019 unit matrix: **9 passed**.
- Native reconciler matrix: **8 passed**.
- Kubernetes checkpoint provider matrix: **13 passed**.
- Retained cleanup RBAC authorization: **1 passed**.
- Explicit remove regressions retained before deletion: **2 + 1 + 2 passed**.
- Switchover pilot/unit regression matrix: **47 passed**.
- Live native test target: compiled; intentionally ignored until Phase 5
  production routing removes the legacy selector.
- Real owner-GC deletion gate:
  `KUBECONFIG="$HOME/.kube/kuberic-kind-config" cargo test -p kuberic-durable-execution --features kubernetes --test kubernetes_checkpoint_real validates_real_api_cas_watch_compaction_and_ambiguous_recovery -- --nocapture`
  — **passed**, including `KUBERNETES_CHECKPOINT_LIFECYCLE owner_gc=passed`.
