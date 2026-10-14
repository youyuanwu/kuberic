use kuberic_runtime::protocol::command::ProtocolCommand;
use kuberic_runtime::protocol::public_operations::{
    PublicV1AcceptedStatus, PublicV1SessionRenewalStatus,
};
use kuberic_runtime::protocol::types::ResourceUid;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublicV1Scope {
    pub namespace: String,
    pub name: String,
    pub resource_uid: ResourceUid,
}

#[async_trait::async_trait]
pub trait PublicV1HistoryApi: Send + Sync {
    async fn observe_public_v1_status(
        &self,
        scope: &PublicV1Scope,
    ) -> Result<Option<PublicV1AcceptedStatus>, String>;

    async fn persist_public_v1_status(
        &self,
        scope: &PublicV1Scope,
        expected: Option<&PublicV1AcceptedStatus>,
        next: &PublicV1AcceptedStatus,
    ) -> Result<(), String>;

    async fn dispatch_public_v1_history(
        &self,
        scope: &PublicV1Scope,
        expected: &PublicV1AcceptedStatus,
        command: &ProtocolCommand,
    ) -> Result<(), String>;
}

#[async_trait::async_trait]
pub trait PublicV1SessionRenewalApi: Send + Sync {
    async fn observe_public_v1_session_renewal(
        &self,
        scope: &PublicV1Scope,
    ) -> Result<Option<PublicV1SessionRenewalStatus>, String>;

    async fn persist_public_v1_session_renewal(
        &self,
        scope: &PublicV1Scope,
        expected: Option<&PublicV1SessionRenewalStatus>,
        next: &PublicV1SessionRenewalStatus,
    ) -> Result<(), String>;

    async fn complete_public_v1_session_renewal(
        &self,
        _scope: &PublicV1Scope,
        _expected: &PublicV1SessionRenewalStatus,
    ) -> Result<(), String> {
        Err("public-v1 session-renewal completion is unsupported".into())
    }

    async fn dispatch_public_v1_session_renewal(
        &self,
        scope: &PublicV1Scope,
        expected: &PublicV1SessionRenewalStatus,
        command: &ProtocolCommand,
    ) -> Result<(), String>;
}
