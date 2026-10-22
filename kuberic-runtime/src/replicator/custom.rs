//! Independent engine effects for the common retained authority owner.

use super::*;
use crate::protocol::public_operations::{PublicConfiguration, PublicInboundBuildAuthority};

/// Ownership of protected engine callbacks when their response future is lost.
///
/// Neither variant certifies failed-callback repeatability, application
/// callbacks, successful containment, or a completed native join.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallbackOwnership {
    /// The callback only observes work retained by the native owner.
    NativeObserver,
    /// Interruption can leave work owned by the engine until explicit drainage.
    ObjectOwnedOnInterruption,
}

/// Optional controller integration for an independent native engine.
///
/// Installation/revocation concern the exact copy attempt, not voting admission
/// or database receipts. The common host owner retains these calls independently
/// of response waiters. Implementations revalidate authority around native effects
/// and awaits. Successful drainage promises actual joining of owned descendants.
///
/// Protected Close/Abort must notify/cancel companion actions before waiting for
/// drainage: the common owner serializes drainage behind an admitted action.
#[async_trait]
pub trait CustomReplicatorControl: Send + Sync {
    /// Immutable for a creation and restricted to this engine's callbacks.
    /// The conservative default does not certify any callback as repeatable.
    fn callback_ownership(&self) -> CallbackOwnership {
        CallbackOwnership::ObjectOwnedOnInterruption
    }
    async fn install_inbound_build_authority(
        &self,
        authority: &PublicInboundBuildAuthority,
    ) -> Result<()>;
    async fn revoke_inbound_build_authority(
        &self,
        authority: &PublicInboundBuildAuthority,
    ) -> Result<()>;
    /// Observe actual idle peers coherently with the installed host
    /// configuration. Unavailable diagnostics must return an error.
    async fn idle_replicas(&self, configuration: &PublicConfiguration) -> Result<Vec<ReplicaId>>;
    /// Invalidate ingress/access and join all real native descendants.
    async fn shutdown_and_drain(&self) -> Result<()>;
}

struct Effects(Arc<dyn CustomReplicatorControl>);

#[async_trait]
impl super::coordinator::Effects for Effects {
    fn callback_ownership(&self) -> CallbackOwnership {
        self.0.callback_ownership()
    }
    async fn install_inbound_build_authority(
        &self,
        authority: &PublicInboundBuildAuthority,
    ) -> Result<()> {
        self.0.install_inbound_build_authority(authority).await
    }
    async fn revoke_inbound_build_authority(
        &self,
        authority: &PublicInboundBuildAuthority,
    ) -> Result<()> {
        self.0.revoke_inbound_build_authority(authority).await
    }
    async fn idle_replicas(&self, configuration: &PublicConfiguration) -> Result<Vec<ReplicaId>> {
        self.0.idle_replicas(configuration).await
    }
    async fn shutdown_and_drain(&self) -> Result<()> {
        self.0.shutdown_and_drain().await
    }
}

pub(super) fn control(
    context: ReplicatorFactoryContext,
    application: Arc<dyn CustomReplicatorControl>,
) -> super::coordinator::Control {
    super::coordinator::Control::new(context, Arc::new(Effects(application)))
}
