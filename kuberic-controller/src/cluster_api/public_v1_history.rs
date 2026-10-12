use kuberic_runtime::protocol::command::ProtocolCommand;
use kuberic_runtime::protocol::public_operations::{
    PublicV1AcceptedStatus, PublicV1SessionRenewalStatus,
};

#[async_trait::async_trait]
pub trait PublicV1HistoryApi: Send + Sync {
    async fn observe_public_v1_status(&self) -> Result<Option<PublicV1AcceptedStatus>, String>;

    async fn persist_public_v1_status(
        &self,
        expected: Option<&PublicV1AcceptedStatus>,
        next: &PublicV1AcceptedStatus,
    ) -> Result<(), String>;

    async fn dispatch_public_v1_history(
        &self,
        expected: &PublicV1AcceptedStatus,
        command: &ProtocolCommand,
    ) -> Result<(), String>;
}

#[async_trait::async_trait]
pub trait PublicV1SessionRenewalApi: Send + Sync {
    async fn observe_public_v1_session_renewal(
        &self,
    ) -> Result<Option<PublicV1SessionRenewalStatus>, String>;

    async fn persist_public_v1_session_renewal(
        &self,
        expected: Option<&PublicV1SessionRenewalStatus>,
        next: &PublicV1SessionRenewalStatus,
    ) -> Result<(), String>;

    async fn dispatch_public_v1_session_renewal(
        &self,
        expected: &PublicV1SessionRenewalStatus,
        command: &ProtocolCommand,
    ) -> Result<(), String>;
}
