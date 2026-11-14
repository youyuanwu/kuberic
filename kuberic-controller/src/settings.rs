use crate::FmAuthority;

/// Controller bootstrap inputs, not persisted KubericSet authority.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Settings {
    pub namespace: String,
    pub fm: FmAuthority,
    pub node_port: u16,
    /// Bounds each accepted bootstrap-completion wait and subsequent teardown.
    pub teardown_timeout_ms: u64,
}
