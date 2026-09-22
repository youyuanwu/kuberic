use kuberic_wire::proto;

use crate::application::Lsn;
use crate::authority::BuildAuthority;

#[derive(Debug, Clone)]
pub struct PreparedCopy {
    pub authority: BuildAuthority,
    pub items: Vec<proto::CopyItem>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildProgress {
    pub authority: BuildAuthority,
    pub last_sequence: u64,
    pub durable_lsn: Lsn,
    pub completed: bool,
}
