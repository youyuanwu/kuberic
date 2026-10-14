//! Opt-in isolated replica fixtures for application conformance tests.
//!
//! Fixture records are detached, serializable descriptions. The opaque fixture
//! hosts and stores cannot be extracted from a running production host, and
//! none of these records can be passed to production admission APIs. Private
//! attachment, registration, lifecycle, and authority capabilities stay private.

mod public_v1_conformance;
pub mod session;
pub mod sqlite_store;
pub mod state;

use serde::{Serialize, de::DeserializeOwned};

fn convert<T: Serialize, U: DeserializeOwned>(value: T) -> U {
    serde_json::from_value(serde_json::to_value(value).expect("serialize fixture record"))
        .expect("fixture record matches the durable host format")
}

pub use public_v1_conformance::{
    ReplacementOwnershipInventory, assert_public_v1_conformance_gate,
    assert_public_v1_conformance_not_applicable, assert_public_v1_conformance_subject,
    execute_public_v1_conformance_subject, public_v1_postgres_conformance_scenarios,
    replacement_ownership_inventory, replacement_ownership_inventory_with_extra_endpoint_fixture,
    validate_replacement_ownership_inventory,
};
