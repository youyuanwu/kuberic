//! Opt-in isolated replica fixtures for application conformance tests.
//!
//! Fixture records are detached, serializable descriptions. The opaque fixture
//! hosts and stores cannot be extracted from a running production host, and
//! none of these records can be passed to production admission APIs. Private
//! attachment, registration, lifecycle, and authority capabilities stay private.

#[cfg(kuberic_workspace_tests)]
mod public_v1_alternate_registration_fixture;
#[cfg(kuberic_workspace_tests)]
mod public_v1_conformance;
pub mod session;
pub mod sqlite_store;
pub mod state;

pub use crate::host::public_lifecycle::{PublicCutPosition, PublicOperationCut};

use serde::{Serialize, de::DeserializeOwned};

/// Conservatively omits the specific-quorum advertisement before host attachment.
/// The actual control/state bundle and its opaque native ownership are unchanged.
pub fn without_catch_up_specific_quorum(
    interfaces: crate::replicator::ReplicatorInterfaces,
) -> crate::Result<crate::replicator::ReplicatorInterfaces> {
    interfaces.without_catch_up_specific_quorum()
}

fn convert<T: Serialize, U: DeserializeOwned>(value: T) -> U {
    serde_json::from_value(serde_json::to_value(value).expect("serialize fixture record"))
        .expect("fixture record matches the durable host format")
}

#[cfg(kuberic_workspace_tests)]
pub use public_v1_conformance::{
    ReplacementOwnershipInventory, assert_public_v1_conformance_gate,
    assert_public_v1_conformance_not_applicable, assert_public_v1_conformance_partition_coverage,
    assert_public_v1_conformance_subject, execute_public_v1_conformance_partition,
    execute_public_v1_conformance_subject, public_v1_postgres_conformance_scenarios,
    replacement_ownership_inventory,
    replacement_ownership_inventory_with_alternate_controller_fixture,
    replacement_ownership_inventory_with_alternate_factory_fixture,
    replacement_ownership_inventory_with_alternate_protocol_fixture,
    replacement_ownership_inventory_with_extra_endpoint_fixture,
    validate_replacement_ownership_inventory,
};
