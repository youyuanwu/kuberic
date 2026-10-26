pub mod access;
pub mod adapter;
pub mod build;
pub mod config;
pub mod data_service;
pub mod durable;
mod generation;
pub mod instance;
pub mod monitor;
pub mod native;
mod owned_command;
mod owned_process;
pub mod owned_task;
#[doc(hidden)]
pub mod process_supervisor;
pub mod service;

pub use service::{PgService, PgServiceConfig};

pub mod proto {
    tonic::include_proto!("pgdata.v2");
}

#[cfg(all(test, feature = "testing"))]
mod testing;

#[cfg(all(test, feature = "testing"))]
mod tests {
    #[test_log::test(tokio::test)]
    async fn public_v1_postgres_secondary_readiness() {
        crate::adapter::public_v1::tests::p6::public_v1_postgres_secondary_readiness().await;
    }

    #[test_log::test(tokio::test)]
    async fn public_v1_postgres_partition_authority() {
        crate::adapter::public_v1::tests::p6::public_v1_postgres_partition_authority().await;
    }

    #[test_log::test(tokio::test)]
    async fn public_v1_postgres_orderly_progress() {
        crate::adapter::public_v1::tests::p6::public_v1_postgres_orderly_progress().await;
    }

    #[test_log::test(tokio::test)]
    async fn public_v1_postgres_marked_catchup() {
        crate::adapter::public_v1::tests::p6::public_v1_postgres_marked_catchup().await;
    }

    #[test_log::test(tokio::test)]
    async fn public_v1_postgres_selected_resources() {
        crate::adapter::public_v1::tests::public_v1_postgres_selected_resources().await;
    }

    #[test_log::test(tokio::test)]
    async fn public_v1_postgres_retained_abort() {
        crate::adapter::public_v1::tests::public_v1_postgres_retained_abort().await;
    }

    #[test_log::test(tokio::test)]
    async fn public_v1_postgres_primary_lifecycle_and_joint_configuration() {
        const SCENARIO: &str = "public_v1_postgres_primary_lifecycle_and_joint_configuration";
        crate::adapter::public_v1::tests::
            public_v1_postgres_primary_lifecycle_and_joint_configuration()
            .await;
        for id in ["KSF-01", "KSF-03", "KSF-04"] {
            kuberic_runtime::testing::assert_public_v1_conformance_gate("postgresql", id, SCENARIO);
        }
        for id in ["KSF-10", "KSF-11"] {
            kuberic_runtime::testing::assert_public_v1_conformance_not_applicable("postgresql", id);
        }
    }

    #[test_log::test(tokio::test)]
    async fn public_v1_postgres_build_cancellation_and_retirement() {
        const SCENARIO: &str = "public_v1_postgres_build_cancellation_and_retirement";
        crate::adapter::public_v1::tests::public_v1_postgres_build_cancellation_and_retirement()
            .await;
        for id in ["KSF-05", "KSF-06", "KSF-12"] {
            kuberic_runtime::testing::assert_public_v1_conformance_gate("postgresql", id, SCENARIO);
        }
    }

    #[test_log::test(tokio::test)]
    async fn public_v1_postgres_data_loss_history_and_replay() {
        const SCENARIO: &str = "public_v1_postgres_data_loss_history_and_replay";
        crate::adapter::public_v1::tests::public_v1_postgres_data_loss_history_and_replay().await;
        kuberic_runtime::testing::assert_public_v1_conformance_gate(
            "postgresql",
            "KSF-08",
            SCENARIO,
        );
        for id in ["KSF-02", "KSF-15"] {
            kuberic_runtime::testing::assert_public_v1_conformance_not_applicable("postgresql", id);
        }
    }

    #[test_log::test(tokio::test)]
    async fn public_v1_postgres_contained_close() {
        const SCENARIO: &str = "public_v1_postgres_contained_close";
        crate::adapter::public_v1::tests::public_v1_postgres_contained_close().await;
        kuberic_runtime::testing::assert_public_v1_conformance_gate(
            "postgresql",
            "KSF-14",
            SCENARIO,
        );
    }

    #[test_log::test(tokio::test)]
    async fn public_v1_postgres_basebackup_cancellation_and_retry() {
        crate::adapter::public_v1::tests::public_v1_postgres_basebackup_cancellation_and_retry()
            .await;
    }

    #[test_log::test(tokio::test)]
    async fn public_v1_postgres_stale_completion_cannot_mutate_successor() {
        crate::adapter::public_v1::tests::
            public_v1_postgres_stale_completion_cannot_mutate_successor()
            .await;
    }

    #[test_log::test(tokio::test)]
    async fn public_v1_postgres_production_factory_and_fault_boundary() {
        crate::adapter::public_v1::tests::public_v1_postgres_production_factory_and_fault_boundary(
        )
        .await;
    }

    #[test_log::test(tokio::test)]
    async fn public_v1_postgres_failover_switchover_and_replacement() {
        crate::adapter::public_v1::tests::public_v1_postgres_failover_switchover_and_replacement()
            .await;
    }
}
