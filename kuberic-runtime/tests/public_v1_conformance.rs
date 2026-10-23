#![cfg(kuberic_workspace_tests)]

#[test]
fn public_v1_conformance_oracle_has_exactly_ksf_01_through_ksf_16() {
    kuberic_runtime::testing::assert_public_v1_conformance_subject("built-in-replacement");
    assert_eq!(
        kuberic_runtime::testing::public_v1_postgres_conformance_scenarios(),
        vec![
            "public_v1_postgres_build_cancellation_and_retirement",
            "public_v1_postgres_contained_close",
            "public_v1_postgres_data_loss_history_and_replay",
            "public_v1_postgres_primary_lifecycle_and_joint_configuration",
        ]
    );

    let inventory = kuberic_runtime::testing::replacement_ownership_inventory();
    kuberic_runtime::testing::validate_replacement_ownership_inventory(&inventory).unwrap();
    let mutation =
        kuberic_runtime::testing::replacement_ownership_inventory_with_extra_endpoint_fixture();
    assert_eq!(
        &mutation.services[..inventory.services.len()],
        inventory.services.as_slice(),
        "endpoint mutation changed the reviewed service manifest"
    );
    assert_eq!(
        mutation.services.last().map(String::as_str),
        Some("unmanifested runtime service registration")
    );
    assert!(
        kuberic_runtime::testing::validate_replacement_ownership_inventory(&mutation).is_err(),
        "KSF-16 canonical eligibility accepted an unmanifested reachable endpoint"
    );
    let mutation =
        kuberic_runtime::testing::replacement_ownership_inventory_with_alternate_factory_fixture();
    assert_eq!(
        &mutation.factories[..inventory.factories.len()],
        inventory.factories.as_slice(),
        "factory mutation changed the reviewed factory manifest"
    );
    assert_eq!(
        mutation.factories.last().map(String::as_str),
        Some("unmanifested built-in factory construction")
    );
    assert!(
        kuberic_runtime::testing::validate_replacement_ownership_inventory(&mutation).is_err(),
        "KSF-16 canonical eligibility accepted an unmanifested reachable factory"
    );
    let mutation =
        kuberic_runtime::testing::replacement_ownership_inventory_with_alternate_protocol_fixture();
    assert!(
        kuberic_runtime::testing::validate_replacement_ownership_inventory(&mutation).is_err(),
        "KSF-16 mutation oracle accepted an alternate protocol"
    );
    let mutation =
        kuberic_runtime::testing::replacement_ownership_inventory_with_alternate_controller_fixture(
        );
    assert_eq!(
        &mutation.controller_modules[..inventory.controller_modules.len()],
        inventory.controller_modules.as_slice(),
        "controller-helper mutation changed the reviewed controller manifest"
    );
    assert_eq!(
        mutation.controller_modules.last().map(String::as_str),
        Some("unmanifested controller startup helper")
    );
    assert!(
        kuberic_runtime::testing::validate_replacement_ownership_inventory(&mutation).is_err(),
        "KSF-16 canonical eligibility accepted an unmanifested reachable controller helper"
    );
}

#[test]
fn public_v1_conformance_partitions_cover_every_registered_observation_once() {
    kuberic_runtime::testing::assert_public_v1_conformance_partition_coverage();
}

#[test]
#[ignore = "bounded native/controller source execution partition"]
fn public_v1_conformance_executes_built_in_replacement_scenarios() {
    kuberic_runtime::testing::execute_public_v1_conformance_subject("built-in-replacement");
}

#[test]
#[ignore = "bounded source partition for the real service recreation journey"]
fn public_v1_conformance_executes_service_failover_after_durable_promotion() {
    kuberic_runtime::testing::execute_public_v1_conformance_partition(
        "built-in-replacement",
        "failover-after-durable-promotion",
    );
}

#[test]
#[ignore = "bounded source partition for the real service recreation journey"]
fn public_v1_conformance_executes_service_failover_at_persisted_promotion() {
    kuberic_runtime::testing::execute_public_v1_conformance_partition(
        "built-in-replacement",
        "failover-at-persisted-promotion",
    );
}

#[test]
#[ignore = "bounded source partition for the real service ambiguity/abort journey"]
fn public_v1_conformance_executes_service_ambiguous_commit_abort() {
    kuberic_runtime::testing::execute_public_v1_conformance_partition(
        "built-in-replacement",
        "ambiguous-commit-abort",
    );
}

#[test]
#[ignore = "bounded source partition for the real service handoff journey"]
fn public_v1_conformance_executes_service_switchover_after_catch_up() {
    kuberic_runtime::testing::execute_public_v1_conformance_partition(
        "built-in-replacement",
        "switchover-after-catch-up",
    );
}

#[test]
#[ignore = "bounded source partition for the real service handoff journey"]
fn public_v1_conformance_executes_service_switchover_after_durable_handoff() {
    kuberic_runtime::testing::execute_public_v1_conformance_partition(
        "built-in-replacement",
        "switchover-after-durable-handoff",
    );
}

#[test]
fn built_in_replacement_ksf_mappings_require_each_authorized_observation() {
    for (gate, scenarios) in [
        (
            "KSF-01",
            &[
                "replicator::v1::role::tests::initial_primary_queries_provider_before_epoch_and_requires_configuration",
                "native_build_callback_receives_invalid_progress_and_exact_frozen_idle_target",
                "retained_secondary_promotion_preserves_acknowledged_bytes_and_replaces_service_location",
                "sf10_quiet_snapshot_build_activates_without_writes_or_fabricated_progress",
            ][..],
        ),
        (
            "KSF-02",
            &[
                "hosted_public_v1_authorized_data_loss_preserves_effective_outcomes_and_history_scope",
            ][..],
        ),
        (
            "KSF-03",
            &[
                "native_same_role_secondary_epoch_fences_predecessor_binding_and_sending_epoch",
                "public_v1_peer_withdrawal_advances_epoch_without_role_replay",
            ][..],
        ),
        (
            "KSF-04",
            &[
                "sf10_quiet_snapshot_build_activates_without_writes_or_fabricated_progress",
                "three_replica_native_quorum_ignores_hung_minority_and_rejects_secondary_origin",
                "native_configuration_dispatch_rejects_self_idle_and_nonmember_descriptors",
            ][..],
        ),
        (
            "KSF-05",
            &[
                "native_build_callback_receives_invalid_progress_and_exact_frozen_idle_target",
                "hosted_operation_stream_copy_and_eof_wait_for_service_owned_snapshot_ack",
                "successor_inbound_build_retires_native_receiver_instead_of_resetting_metadata",
                "native_build_binding_pins_frozen_source_address_even_when_source_is_unreachable",
                "public_copy_context::public_build_consumes_target_context_and_installs_nonempty_snapshot",
            ][..],
        ),
        (
            "KSF-06",
            &[
                "native_remove_cancels_session_without_late_install_and_close_drains_retained_provider",
                "hung_native_build_is_drained_by_successor_close_without_a_fake_rpc_deadline",
                "hosted_sf06_close_waits_for_synchronous_service_effect_even_if_observer_is_dropped",
                "hosted_operation_stream_close_and_abort_drain_with_dequeued_or_unconsumed_work",
                "exact_idle_removal_does_not_reauthorize_retired_service_work",
            ][..],
        ),
        (
            "KSF-07",
            &["native_swap_freezes_specific_quorum_and_conservative_all_fallback_inputs"][..],
        ),
        (
            "KSF-08",
            &[
                "public_v1_history_partition_seed_is_shared",
                "native_query_outcome_matrix_survives_rpc_persistence_and_controller_eligibility",
                "native_query_not_queried_preserves_copy_detail_and_cannot_elect_or_build",
                "native_query_rejoin_matrix_distinguishes_source_capability_from_target_current",
                "native_query_outcomes_remain_independent_through_host_wire_and_controller_eligibility",
                "hosted_public_v1_authorized_data_loss_preserves_effective_outcomes_and_history_scope",
                "read_fencing::sf_d02_native_quorum_progress_is_not_a_service_commit_or_caller_cancellation_oracle",
                "read_fencing::sf11_acknowledged_prefix_6_through_10_survives_ambiguous_11_without_history_fabrication",
                "production_carrier_rejects_crossed_process_storage_history_epoch_and_endpoint",
            ][..],
        ),
        (
            "KSF-09",
            &["production_config_has_no_protocol_selector_or_application_commit_oracle"][..],
        ),
        (
            "KSF-10",
            &[
                "public_v1_open_owns_one_registration_listener_and_writer_and_close_failure_is_retained",
            ][..],
        ),
        (
            "KSF-11",
            &[
                "retained_secondary_promotion_preserves_acknowledged_bytes_and_replaces_service_location",
                "public_factory_primary_process_renewal_retains_exact_role_address_authority",
            ][..],
        ),
        (
            "KSF-12",
            &[
                "hosted_sf06_close_waits_for_synchronous_service_effect_even_if_observer_is_dropped",
                "successor_copy_requires_replacement_without_exposing_held_service_install",
                "hung_native_build_is_drained_by_successor_close_without_a_fake_rpc_deadline",
            ][..],
        ),
        (
            "KSF-13",
            &[
                "native_transient_fault_restarts_same_storage_under_exact_session_renewal",
                "native_controller_unreachable_survivor_fences_until_exact_return_or_explicit_retirement",
                "primary_commit_failure_contains_the_ambiguous_native_quorum_success",
                "successor_copy_requires_replacement_without_exposing_held_service_install",
                "service_rejects_corrupt_copy_and_conflicting_retained_history_before_ack",
                "retained_service_state_reopens_without_replaying_interrupted_application_children",
            ][..],
        ),
        (
            "KSF-14",
            &[
                "public_v1_open_owns_one_registration_listener_and_writer_and_close_failure_is_retained",
                "hung_native_build_is_drained_by_successor_close_without_a_fake_rpc_deadline",
                "terminal_close_restarts_fenced_without_replaying_application_open",
            ][..],
        ),
        (
            "KSF-15",
            &[
                "in_process::journeys::sf01_primary_local_commit_isolation_and_abort_never_late_grants_success",
                "in_process::journeys::failover_recreation_at_persisted_promotion_preserves_effects",
                "in_process::journeys::failover_recreation_after_durable_promotion_consumes_completion_once",
                "in_process::journeys::switchover_recreation_after_catch_up_resumes_accepted_handoff",
                "in_process::journeys::switchover_recreation_after_durable_handoff_resumes_promotion",
                "public_v1_open_owns_one_registration_listener_and_writer_and_close_failure_is_retained",
                "exact_idle_removal_does_not_reauthorize_retired_service_work",
                "terminal_close_restarts_fenced_without_replaying_application_open",
                "public_v1_replacement_schema_failed_build_containment_survives_reopen",
                "public_v1_replacement_schema_rebuild_then_cancellation_survives_reopen",
                "public_v1_history_data_loss_attempt_renews_higher_epoch",
                "public_v1_data_loss_renewal_requires_exact_replay_capability",
                "public_v1_replacement_schema_build_ambiguity_requires_retirement",
            ][..],
        ),
        (
            "KSF-16",
            &[
                "three_replica_native_quorum_ignores_hung_minority_and_rejects_secondary_origin",
                "production_carrier_rejects_crossed_process_storage_history_epoch_and_endpoint",
                "native_carrier_bad_payload_and_wrong_sender_cannot_publish_service_progress",
                "native_replication_body_epochs_cannot_escape_a_valid_admitted_binding",
                "native_build_start_and_copy_body_epochs_are_checked_before_receiver_mutation",
                "native_build_binding_pins_frozen_source_address_even_when_source_is_unreachable",
                "successor_inbound_build_retires_native_receiver_instead_of_resetting_metadata",
            ][..],
        ),
    ] {
        for scenario in scenarios {
            kuberic_runtime::testing::assert_public_v1_conformance_gate(
                "built-in-replacement",
                gate,
                scenario,
            );
        }
    }
}
