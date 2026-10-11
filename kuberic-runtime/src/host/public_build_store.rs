use serde::{Deserialize, Serialize};

use crate::protocol::authorization::{
    AuthorizationAuditDigest, AuthorizationKeyId, AuthorizationVerificationKey,
    BuildAuthorizationEndpointV1, BuildAuthorizationV1,
};
use crate::protocol::public_operations::PublicV1OperationResult;
use crate::protocol::types::{Epoch, HistoryContext, OperationId};

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
    pub(crate) verification_key: AuthorizationVerificationKey,
    pub(crate) key_id: AuthorizationKeyId,
    pub(crate) audit_digest: AuthorizationAuditDigest,
    pub(crate) history: HistoryContext,
    pub(crate) epoch: Epoch,
    pub(crate) source: BuildAuthorizationEndpointV1,
    pub(crate) target: BuildAuthorizationEndpointV1,
    pub(crate) authorization: BuildAuthorizationV1,
    pub(crate) retirement: PublicV1BuildRetirement,
    pub(crate) result: Option<PublicV1OperationResult>,
}

impl PublicV1BuildRecord {
    pub(crate) fn validate(&self) -> Result<(), &'static str> {
        if self.operation_id.is_empty()
            || self.build_id.is_empty()
            || self.admission_revision == 0
            || self.operation_id != self.build_id
            || self.key_id != self.verification_key.key_id()
            || self.authorization.claims.operation_id != self.operation_id
            || self.authorization.claims.build_id != self.build_id
            || self.authorization.claims.key_id != self.key_id
            || self.authorization.claims.history != self.history
            || self.authorization.claims.epoch != self.epoch
            || self.authorization.claims.source != self.source
            || self.authorization.claims.target != self.target
            || self
                .authorization
                .audit_digest()
                .map_err(|_| "public-v1 build record has an invalid authorization envelope")?
                != self.audit_digest
        {
            return Err("public-v1 build record differs from signed authority");
        }
        self.verification_key
            .verify_build_authorization(&self.authorization)
            .map_err(|_| "public-v1 build record signature is invalid")?;
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
