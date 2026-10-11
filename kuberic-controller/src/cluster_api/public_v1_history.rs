use kuberic_runtime::protocol::command::ProtocolCommand;
use kuberic_runtime::protocol::public_operations::PublicV1AcceptedStatus;

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
