//! Versioned transport contracts for the level-triggered Kuberic stack.
//!
//! Generated protobuf types remain transport-only. Validation and conversion
//! establish canonical `kuberic-protocol` authority before callers use them.

pub mod convert;

pub mod proto {
    tonic::include_proto!("kuberic.level.v1");
}

pub use convert::{
    WireError, ensure_supported_version, normalize_agent_status_report,
    validate_agent_status_report, validate_execute_request, validate_replication_ack,
    validate_replication_item,
};
