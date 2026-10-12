use serde::{Deserialize, Serialize};

use crate::protocol::public_operations::PublicV1OperationResult;
use crate::protocol::types::{
    Epoch, HistoryContext, OperationId, ProcessSessionId, ReplicaIdentity,
    ReplicaStorageIncarnationContext,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum PublicV1BuildRetirement {
    Retain,
    Required,
    Retired,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PublicV1BuildRecord {
    pub(crate) operation_id: OperationId,
    pub(crate) build_id: OperationId,
    pub(crate) admission_revision: u64,
    pub(crate) history: HistoryContext,
    pub(crate) epoch: Epoch,
    pub(crate) source: ReplicaIdentity,
    pub(crate) source_storage: ReplicaStorageIncarnationContext,
    pub(crate) source_session: ProcessSessionId,
    pub(crate) target: ReplicaIdentity,
    pub(crate) target_storage: ReplicaStorageIncarnationContext,
    pub(crate) target_session: ProcessSessionId,
    pub(crate) target_address: String,
    pub(crate) retirement: PublicV1BuildRetirement,
    pub(crate) result: Option<PublicV1OperationResult>,
}

impl PublicV1BuildRecord {
    pub(crate) fn validate(&self) -> Result<(), &'static str> {
        if self.operation_id.is_empty()
            || self.build_id.is_empty()
            || self.admission_revision == 0
            || self.operation_id != self.build_id
            || self.source_session.is_empty()
            || self.target_session.is_empty()
            || self.target_address.is_empty()
            || self.source == self.target
        {
            return Err("public-v1 build record has incomplete exact attempt identity");
        }
        self.history.validate()?;
        self.source_storage.validate()?;
        self.target_storage.validate()?;
        if self.source_storage.replica != self.source || self.target_storage.replica != self.target
        {
            return Err("public-v1 build storage differs from its replica");
        }
        match self.result.as_ref() {
            Some(
                PublicV1OperationResult::Failed(detail)
                | PublicV1OperationResult::Ambiguous(detail),
            ) if detail.is_empty() => {
                return Err("public-v1 build failure lacks detail");
            }
            Some(PublicV1OperationResult::RebuildRequired(rebuild)) => rebuild.validate()?,
            _ => {}
        }
        match (&self.result, self.retirement) {
            (None, PublicV1BuildRetirement::Retain)
            | (Some(PublicV1OperationResult::Succeeded), PublicV1BuildRetirement::Retain)
            | (
                Some(
                    PublicV1OperationResult::Failed(_)
                    | PublicV1OperationResult::Ambiguous(_)
                    | PublicV1OperationResult::Cancelled
                    | PublicV1OperationResult::RebuildRequired(_),
                ),
                PublicV1BuildRetirement::Required,
            )
            | (Some(_), PublicV1BuildRetirement::Retired) => Ok(()),
            _ => Err("public-v1 build result and retirement state are inconsistent"),
        }
    }
}
