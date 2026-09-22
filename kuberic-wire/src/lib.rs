pub mod convert;

pub mod proto {
    tonic::include_proto!("kuberic.level.v1");
}

pub use convert::{
    WireError, ensure_supported_version, normalize_agent_status_report,
    validate_agent_status_report, validate_execute_request, validate_replication_ack,
    validate_replication_item,
};
