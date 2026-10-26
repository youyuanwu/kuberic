#![cfg(feature = "process-members")]

#[path = "../src/retained_applications.rs"]
pub mod retained_applications;
#[path = "../../scripts/fixtures/controller-test-util/src/retained_witness.rs"]
pub mod retained_witness;

use retained_witness::{WitnessFactory, helper_path, qualify_retained_secondary};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn real_kv_failed_owner_restores_without_copy_and_remains_consuming_secondary()
-> anyhow::Result<()> {
    qualify_retained_secondary(WitnessFactory::new(
        retained_applications::KvFactory,
        helper_path(
            "KUBERIC_RETAINED_KV_MEMBER_BIN",
            "kuberic-retained-kv-member",
        ),
    ))
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn real_sqlite_failed_owner_restores_without_copy_and_remains_consuming_secondary()
-> anyhow::Result<()> {
    qualify_retained_secondary(WitnessFactory::new(
        retained_applications::SqliteFactory,
        helper_path(
            "KUBERIC_RETAINED_SQLITE_MEMBER_BIN",
            "kuberic-retained-sqlite-member",
        ),
    ))
    .await
}
