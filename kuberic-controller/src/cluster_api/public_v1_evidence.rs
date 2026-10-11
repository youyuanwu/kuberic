use kuberic_runtime::protocol::public_operations::{
    CanonicalReplicaRebuildRequired, PublicV1AcceptedStatus, PublicV1OperationCompletion,
};
use kuberic_runtime::protocol::types::ReplicaStorageIncarnationContext;

#[async_trait::async_trait]
pub trait PublicV1EvidenceApi: Send + Sync {
    async fn observe_public_v1_evidence_status(
        &self,
    ) -> Result<Option<PublicV1AcceptedStatus>, String>;

    async fn persist_public_v1_evidence_status(
        &self,
        expected: &PublicV1AcceptedStatus,
        next: &PublicV1AcceptedStatus,
    ) -> Result<(), String>;

    async fn accept_public_v1_completion(
        &self,
        expected: &PublicV1AcceptedStatus,
        completion: &PublicV1OperationCompletion,
    ) -> Result<(), String>;

    async fn retire_public_v1_incarnation(
        &self,
        expected: &PublicV1AcceptedStatus,
        completion: &PublicV1OperationCompletion,
        storage: &ReplicaStorageIncarnationContext,
    ) -> Result<(), String>;

    async fn provision_public_v1_replacement(
        &self,
        expected: &PublicV1AcceptedStatus,
        completion: &PublicV1OperationCompletion,
        rebuild: &CanonicalReplicaRebuildRequired,
    ) -> Result<(), String>;
}
