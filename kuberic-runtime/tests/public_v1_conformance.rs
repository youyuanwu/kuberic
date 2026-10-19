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
#[ignore = "bounded nested conformance execution gate"]
fn public_v1_conformance_executes_built_in_replacement_scenarios() {
    kuberic_runtime::testing::execute_public_v1_conformance_subject("built-in-replacement");
}

#[test]
fn built_in_replacement_ksf_mappings_require_each_authorized_observation() {
    for (gate, scenarios) in [
        (
            "KSF-02",
            &["hosted_public_v1_authorized_data_loss_maps_only_the_effective_outcome"][..],
        ),
        (
            "KSF-06",
            &[
                "hung_build_peer_is_bounded_and_drained_before_successor_close",
                "hosted_public_v1_build_dispatches_exact_attempt_and_waits_for_matching_completion",
                "hosted_public_v1_removal_drains_build_excludes_peer_and_rejects_stale_session",
            ][..],
        ),
        (
            "KSF-08",
            &[
                "public_v1_history_partition_seed_is_shared",
                "ambiguous_suffix_exactly_replays_quorum_peers_despite_higher_advertised_progress",
                "hosted_public_v1_switchover_dispatches_exact_intent_and_accepts_matching_completion",
            ][..],
        ),
        (
            "KSF-09",
            &[
                "production_config_has_no_protocol_selector_and_stateless_source_has_no_legacy_store_or_auth",
            ][..],
        ),
        (
            "KSF-10",
            &["public_v1_open_owns_registration_listener_and_writer_exactly_once"][..],
        ),
        (
            "KSF-11",
            &["hosted_public_v1_role_address_publishes_replaces_and_clears"][..],
        ),
        (
            "KSF-12",
            &[
                "hosted_public_v1_supersession_cancels_build_root_and_drains_descendant_before_close",
            ][..],
        ),
        (
            "KSF-13",
            &[
                "hosted_public_v1_unreachable_survivor_blocks_access_until_return_or_retirement",
                "hosted_public_v1_rebuild_retires_and_restarts_fresh_replacement",
                "hosted_public_v1_switchover_dispatches_exact_intent_and_accepts_matching_completion",
            ][..],
        ),
        (
            "KSF-14",
            &["hosted_public_v1_contained_close_restarts_without_replaying_interrupted_children"][..],
        ),
        (
            "KSF-15",
            &[
                "hosted_public_v1_contained_close_restarts_without_replaying_interrupted_children",
                "public_v1_replacement_schema_failed_build_containment_survives_reopen",
                "public_v1_replacement_schema_rebuild_then_cancellation_survives_reopen",
                "public_v1_history_data_loss_attempt_renews_higher_epoch",
                "public_v1_data_loss_renewal_requires_exact_replay_capability",
                "public_v1_replacement_schema_build_ambiguity_requires_retirement",
            ][..],
        ),
        (
            "KSF-16",
            &["replication_ingress_accepts_only_exact_current_primary_in_three_replica_set"][..],
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
