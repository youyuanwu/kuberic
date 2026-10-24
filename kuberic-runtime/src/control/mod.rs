//! Versioned transport contracts for the level-triggered Kuberic stack.
//!
//! Generated protobuf types remain transport-only. Validation and conversion
//! establish canonical [`crate::protocol`] authority before callers use them.

pub mod convert;
mod discovery;
mod scale_down;
mod scale_up;
pub(crate) use discovery::resolve_replica_publication;

pub mod proto {
    tonic::include_proto!("kuberic.level.v1");
}

pub use convert::{
    ExecuteEnvelope, WireError, ensure_supported_version, normalize_execute_request,
    normalize_public_v1_agent_status_report, normalize_uninitialized_agent_status_report,
    public_v1_agent_status_to_proto, public_v1_history_command_to_proto,
    public_v1_operation_command_to_proto, validate_execute_request,
};
