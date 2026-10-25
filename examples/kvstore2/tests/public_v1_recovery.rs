#![cfg(feature = "testing")]

#[path = "support/ordinary_recovery.rs"]
mod ordinary_recovery;

#[tokio::test]
async fn m2_singleton_actual_acknowledged_data_restart_has_two_stage_admission() {
    ordinary_recovery::singleton_restart().await;
}

#[tokio::test]
async fn m2_retained_secondary_recovers_with_fresh_session_and_nonregressing_data() {
    ordinary_recovery::retained_secondary_restart().await;
}

#[tokio::test]
async fn m2_retired_recovery_cannot_install_late_copy_or_a_successor() {
    ordinary_recovery::retired_secondary_recovery().await;
}

#[tokio::test]
async fn kvstore2_fresh_restart_build_and_corrective_empty_replacement() {
    ordinary_recovery::fresh_restart_build_and_corrective_empty_replacement().await;
}

#[tokio::test]
async fn kvstore2_real_provider_restart_settlement_and_handoff() {
    ordinary_recovery::real_provider_restart_settlement_and_handoff().await;
}

#[tokio::test]
async fn kvstore2_original_primary_loss_promotes_survivor_and_fences_return() {
    ordinary_recovery::original_primary_loss_promotes_survivor_and_fences_return().await;
}
