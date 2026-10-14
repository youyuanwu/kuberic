//! Public V1 reconciliation decisions over canonical reports and accepted status.

#[cfg(feature = "runtime-test-bridge")]
pub(crate) mod public_lifecycle;
pub mod public_v1_evidence;
pub mod public_v1_history;
pub mod public_v1_session_renewal;
#[cfg(feature = "runtime-test-bridge")]
#[doc(hidden)]
pub mod test_bridge;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvaluationConfig {
    pub stable_resync_seconds: u64,
    pub wait_requeue_seconds: u64,
    pub unsafe_requeue_seconds: u64,
}

impl Default for EvaluationConfig {
    fn default() -> Self {
        Self {
            stable_resync_seconds: 30,
            wait_requeue_seconds: 5,
            unsafe_requeue_seconds: 30,
        }
    }
}
