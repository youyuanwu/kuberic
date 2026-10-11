use k8s_openapi::api::core::v1::Secret;
use kuberic_runtime::protocol::public_operations::{
    MAX_PUBLIC_V1_AUTHORIZATION_INTENTS, PublicV1AcceptedStatus, PublicV1AuthorizationClaims,
    PublicV1AuthorizationCleanupDisposition, PublicV1AuthorizationEnvelope,
    PublicV1AuthorizationIntent, PublicV1AuthorizationIntentKind, PublicV1AuthorizationIntentScope,
    PublicV1AuthorizationIntentStage, PublicV1AuthorizationStatus,
};
use kuberic_runtime::protocol::types::OperationId;

use crate::cluster_api::{
    PublicV1AuthorizationDispatch, PublicV1AuthorizationSecretMaterial,
    PublicV1AuthorizationSecretOwner, public_v1_authorization_secret_binding,
    validate_public_v1_authorization_binding,
};

pub type PublicV1AuthorizationIntentSlot = PublicV1AuthorizationIntentScope;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublicV1AuthorizationRequest {
    pub slot: PublicV1AuthorizationIntentSlot,
    pub operation_id: OperationId,
    pub revision: u64,
    pub claims: PublicV1AuthorizationClaims,
    pub cleanup_disposition: PublicV1AuthorizationCleanupDisposition,
}

impl PublicV1AuthorizationRequest {
    pub fn validate(&self) -> Result<(), String> {
        if self.operation_id.is_empty()
            || self.revision == 0
            || self.cleanup_disposition == PublicV1AuthorizationCleanupDisposition::Retired
        {
            return Err("public-v1 authorization request is incomplete".into());
        }
        let valid_slot = match self.slot {
            PublicV1AuthorizationIntentSlot::Build => self
                .claims
                .build
                .as_deref()
                .is_some_and(|claims| claims.operation_id == self.operation_id),
            PublicV1AuthorizationIntentSlot::CurrentPeer => self
                .claims
                .peer
                .as_deref()
                .is_some_and(|claims| claims.configuration_revision == self.revision),
            PublicV1AuthorizationIntentSlot::PreviousPeer => {
                self.claims.peer.as_deref().is_some_and(|claims| {
                    claims.configuration_revision == self.revision
                        && claims.previous_configuration_id.is_some()
                })
            }
        };
        if !valid_slot {
            return Err(
                "public-v1 authorization request slot or revision differs from its claims".into(),
            );
        }
        PublicV1AuthorizationIntent::prepared(
            self.slot,
            self.operation_id.clone(),
            self.revision,
            self.claims.clone(),
            self.cleanup_disposition,
        )
        .map(|_| ())
        .map_err(str::to_owned)
    }

    fn prepared_intent(&self) -> Result<PublicV1AuthorizationIntent, String> {
        self.validate()?;
        PublicV1AuthorizationIntent::prepared(
            self.slot,
            self.operation_id.clone(),
            self.revision,
            self.claims.clone(),
            self.cleanup_disposition,
        )
        .map_err(str::to_owned)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublicV1AuthorizationEvidence {
    pub operation_id: OperationId,
    pub revision: u64,
    pub kind: PublicV1AuthorizationIntentKind,
    pub audit_digest: kuberic_runtime::protocol::authorization::AuthorizationAuditDigest,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PublicV1AuthorizationPlan {
    CreateSecret {
        expected: Box<PublicV1AcceptedStatus>,
        owner: PublicV1AuthorizationSecretOwner,
    },
    PersistStatus {
        expected: Box<PublicV1AcceptedStatus>,
        next: Box<PublicV1AcceptedStatus>,
        owner: PublicV1AuthorizationSecretOwner,
    },
    Sign {
        expected: Box<PublicV1AcceptedStatus>,
        owner: PublicV1AuthorizationSecretOwner,
        operation_id: OperationId,
    },
    Dispatch {
        expected: Box<PublicV1AcceptedStatus>,
        owner: PublicV1AuthorizationSecretOwner,
        operation_id: OperationId,
    },
    Stable,
}

pub fn plan_public_v1_authorization(
    accepted: &PublicV1AcceptedStatus,
    owner: &PublicV1AuthorizationSecretOwner,
    observed_secret: Option<&Secret>,
    request: Option<&PublicV1AuthorizationRequest>,
    evidence: Option<&PublicV1AuthorizationEvidence>,
) -> Result<PublicV1AuthorizationPlan, String> {
    owner.validate_for_accepted(accepted)?;
    let authorization = accepted.authorization().cloned().unwrap_or_default();
    authorization
        .validate(&accepted.identity, &accepted.history)
        .map_err(str::to_owned)?;

    let Some(binding) = authorization.secret_binding.as_ref() else {
        if request.is_some() || evidence.is_some() {
            return Err("public-v1 authorization intent cannot precede the Secret binding".into());
        }
        let Some(secret) = observed_secret else {
            return Ok(PublicV1AuthorizationPlan::CreateSecret {
                expected: Box::new(accepted.clone()),
                owner: owner.clone(),
            });
        };
        let mut next = accepted.clone();
        next.authorization_mut().secret_binding =
            Some(public_v1_authorization_secret_binding(secret, owner)?);
        next.validate_authorization().map_err(str::to_owned)?;
        return Ok(PublicV1AuthorizationPlan::PersistStatus {
            expected: Box::new(accepted.clone()),
            next: Box::new(next),
            owner: owner.clone(),
        });
    };

    let secret = observed_secret.ok_or_else(|| {
        "bound public-v1 authorization Secret was lost; regeneration is forbidden".to_string()
    })?;
    validate_public_v1_authorization_binding(secret, owner, binding)?;

    let Some(request) = request else {
        if evidence.is_some() {
            return Err("public-v1 authorization evidence lacks an accepted intent".into());
        }
        if authorization
            .intents
            .iter()
            .any(|intent| intent.stage == PublicV1AuthorizationIntentStage::Retired)
        {
            return Ok(PublicV1AuthorizationPlan::PersistStatus {
                expected: Box::new(accepted.clone()),
                next: Box::new(compact_retired_intents(accepted)?),
                owner: owner.clone(),
            });
        }
        return Ok(PublicV1AuthorizationPlan::Stable);
    };
    request.validate()?;
    let prepared = request.prepared_intent()?;
    prepared.validate(Some(binding)).map_err(str::to_owned)?;

    if authorization.intents.iter().any(|intent| {
        intent.stage == PublicV1AuthorizationIntentStage::Retired
            && intent.operation_id == request.operation_id
    }) {
        return Err("retired public-v1 authorization predecessor cannot be replayed".into());
    }
    if authorization
        .intents
        .iter()
        .any(|intent| intent.stage == PublicV1AuthorizationIntentStage::Retired)
    {
        return Ok(PublicV1AuthorizationPlan::PersistStatus {
            expected: Box::new(accepted.clone()),
            next: Box::new(compact_retired_intents(accepted)?),
            owner: owner.clone(),
        });
    }
    if let Some((accepted_slot, intent)) = active_intents_with_slots(&authorization)
        .find(|(_, intent)| intent.operation_id == request.operation_id)
    {
        if accepted_slot != request.slot {
            return Err("accepted public-v1 authorization intent changed slots".into());
        }
        if intent.kind != prepared.kind
            || intent.revision != prepared.revision
            || intent.claims != prepared.claims
            || intent.canonical_claims_base64 != prepared.canonical_claims_base64
            || (intent.cleanup_disposition != prepared.cleanup_disposition
                && intent.stage != PublicV1AuthorizationIntentStage::Retired)
        {
            return Err(
                "accepted public-v1 authorization intent changed identity or claims".into(),
            );
        }
        return match intent.stage {
            PublicV1AuthorizationIntentStage::Prepared => {
                if evidence.is_some() {
                    return Err("unsigned public-v1 authorization cannot have evidence".into());
                }
                Ok(PublicV1AuthorizationPlan::Sign {
                    expected: Box::new(accepted.clone()),
                    owner: owner.clone(),
                    operation_id: request.operation_id.clone(),
                })
            }
            PublicV1AuthorizationIntentStage::Signed => {
                if evidence.is_some() {
                    return Err("undispatched public-v1 authorization cannot have evidence".into());
                }
                Ok(PublicV1AuthorizationPlan::Dispatch {
                    expected: Box::new(accepted.clone()),
                    owner: owner.clone(),
                    operation_id: request.operation_id.clone(),
                })
            }
            PublicV1AuthorizationIntentStage::Dispatched => {
                let Some(evidence) = evidence else {
                    return Ok(PublicV1AuthorizationPlan::Stable);
                };
                validate_evidence(intent, evidence)?;
                Ok(PublicV1AuthorizationPlan::PersistStatus {
                    expected: Box::new(accepted.clone()),
                    next: Box::new(update_intent_stage(
                        accepted,
                        &request.operation_id,
                        PublicV1AuthorizationIntentStage::Observed,
                    )?),
                    owner: owner.clone(),
                })
            }
            PublicV1AuthorizationIntentStage::Observed => {
                if let Some(evidence) = evidence {
                    validate_evidence(intent, evidence)?;
                }
                if intent.cleanup_disposition
                    == PublicV1AuthorizationCleanupDisposition::RetireAfterObservation
                {
                    Ok(PublicV1AuthorizationPlan::PersistStatus {
                        expected: Box::new(accepted.clone()),
                        next: Box::new(retire_intent(accepted, &request.operation_id)?),
                        owner: owner.clone(),
                    })
                } else {
                    Ok(PublicV1AuthorizationPlan::Stable)
                }
            }
            PublicV1AuthorizationIntentStage::Retired => {
                Err("retired public-v1 authorization predecessor cannot be replayed".into())
            }
        };
    }

    if evidence.is_some() {
        return Err("public-v1 authorization evidence names an unaccepted intent".into());
    }
    let mut next = accepted.clone();
    let next_authorization = next.authorization_mut();
    if next_authorization.intents.len() >= MAX_PUBLIC_V1_AUTHORIZATION_INTENTS {
        return Err("public-v1 authorization intent retention is full".into());
    }
    if let Some(index) = matching_slot_index(next_authorization, request)? {
        let current = next_authorization.intents[index].clone();
        if request.revision <= current.revision {
            return Err(
                "public-v1 authorization supersession must increase the accepted revision".into(),
            );
        }
        next_authorization.intents[index] = as_retired(current, binding)?;
    }
    next_authorization.intents.push(prepared);
    next.validate_authorization().map_err(str::to_owned)?;
    Ok(PublicV1AuthorizationPlan::PersistStatus {
        expected: Box::new(accepted.clone()),
        next: Box::new(next),
        owner: owner.clone(),
    })
}

fn active_intents(
    authorization: &PublicV1AuthorizationStatus,
) -> impl Iterator<Item = &PublicV1AuthorizationIntent> {
    authorization
        .intents
        .iter()
        .filter(|intent| intent.stage != PublicV1AuthorizationIntentStage::Retired)
}

fn active_intents_with_slots(
    authorization: &PublicV1AuthorizationStatus,
) -> impl Iterator<
    Item = (
        PublicV1AuthorizationIntentSlot,
        &PublicV1AuthorizationIntent,
    ),
> {
    authorization
        .intents
        .iter()
        .filter(|intent| intent.stage != PublicV1AuthorizationIntentStage::Retired)
        .map(|intent| (intent.scope, intent))
}

fn matching_slot_index(
    authorization: &PublicV1AuthorizationStatus,
    request: &PublicV1AuthorizationRequest,
) -> Result<Option<usize>, String> {
    let key = request
        .claims
        .build
        .as_deref()
        .map(|claims| claims.target.replica())
        .or_else(|| {
            request
                .claims
                .peer
                .as_deref()
                .map(|claims| &claims.secondary.replica)
        })
        .ok_or_else(|| "public-v1 authorization request lacks typed claims".to_string())?;
    Ok(authorization.intents.iter().position(|intent| {
        intent.stage != PublicV1AuthorizationIntentStage::Retired
            && intent.scope == request.slot
            && match request.slot {
                PublicV1AuthorizationIntentSlot::Build => {
                    intent
                        .claims
                        .as_ref()
                        .and_then(PublicV1AuthorizationClaims::build_target)
                        == Some(key)
                }
                PublicV1AuthorizationIntentSlot::CurrentPeer
                | PublicV1AuthorizationIntentSlot::PreviousPeer => {
                    intent
                        .claims
                        .as_ref()
                        .and_then(PublicV1AuthorizationClaims::peer_secondary)
                        == Some(key)
                }
            }
    }))
}

fn find_active_intent_mut<'a>(
    authorization: &'a mut PublicV1AuthorizationStatus,
    operation_id: &OperationId,
) -> Option<&'a mut PublicV1AuthorizationIntent> {
    authorization.intents.iter_mut().find(|intent| {
        intent.stage != PublicV1AuthorizationIntentStage::Retired
            && &intent.operation_id == operation_id
    })
}

fn validate_evidence(
    intent: &PublicV1AuthorizationIntent,
    evidence: &PublicV1AuthorizationEvidence,
) -> Result<(), String> {
    if evidence.operation_id != intent.operation_id
        || evidence.revision != intent.revision
        || evidence.kind != intent.kind
        || Some(evidence.audit_digest) != intent.audit_digest
    {
        return Err("public-v1 authorization evidence differs from retained authority".into());
    }
    Ok(())
}

fn update_intent_stage(
    accepted: &PublicV1AcceptedStatus,
    operation_id: &OperationId,
    stage: PublicV1AuthorizationIntentStage,
) -> Result<PublicV1AcceptedStatus, String> {
    let mut next = accepted.clone();
    let binding = next
        .authorization()
        .and_then(|authorization| authorization.secret_binding.clone())
        .ok_or_else(|| "public-v1 authorization transition lacks a Secret binding".to_string())?;
    let intent = find_active_intent_mut(next.authorization_mut(), operation_id)
        .ok_or_else(|| "public-v1 authorization transition names no active intent".to_string())?;
    let previous = intent.clone();
    intent.stage = stage;
    intent.validate(Some(&binding)).map_err(str::to_owned)?;
    if !intent.is_successor_of(&previous) {
        return Err("public-v1 authorization transition is not monotonic".into());
    }
    next.validate_authorization().map_err(str::to_owned)?;
    Ok(next)
}

fn as_retired(
    mut intent: PublicV1AuthorizationIntent,
    binding: &kuberic_runtime::protocol::public_operations::PublicV1AuthorizationSecretBinding,
) -> Result<PublicV1AuthorizationIntent, String> {
    let previous = intent.clone();
    intent.stage = PublicV1AuthorizationIntentStage::Retired;
    intent.cleanup_disposition = PublicV1AuthorizationCleanupDisposition::Retired;
    intent.claims = None;
    intent.canonical_claims_base64 = None;
    intent.envelope = None;
    intent.signature = None;
    intent.validate(Some(binding)).map_err(str::to_owned)?;
    if !intent.is_successor_of(&previous) {
        return Err("public-v1 authorization retirement is not monotonic".into());
    }
    Ok(intent)
}

fn compact_retired_intents(
    accepted: &PublicV1AcceptedStatus,
) -> Result<PublicV1AcceptedStatus, String> {
    let mut next = accepted.clone();
    next.authorization_mut()
        .intents
        .retain(|intent| intent.stage != PublicV1AuthorizationIntentStage::Retired);
    next.validate_authorization().map_err(str::to_owned)?;
    if !next.is_authorization_successor_of(accepted) {
        return Err("public-v1 authorization compaction is not monotonic".into());
    }
    Ok(next)
}

fn retire_intent(
    accepted: &PublicV1AcceptedStatus,
    operation_id: &OperationId,
) -> Result<PublicV1AcceptedStatus, String> {
    let mut next = accepted.clone();
    let binding = next
        .authorization()
        .and_then(|authorization| authorization.secret_binding.clone())
        .ok_or_else(|| "public-v1 authorization retirement lacks a Secret binding".to_string())?;
    let authorization = next.authorization_mut();
    let intent = find_active_intent_mut(authorization, operation_id)
        .ok_or_else(|| "public-v1 authorization retirement names no active intent".to_string())?;
    *intent = as_retired(intent.clone(), &binding)?;
    next.validate_authorization().map_err(str::to_owned)?;
    Ok(next)
}

pub(crate) fn sign_public_v1_authorization(
    accepted: &PublicV1AcceptedStatus,
    operation_id: &OperationId,
    material: &PublicV1AuthorizationSecretMaterial,
) -> Result<PublicV1AcceptedStatus, String> {
    let mut next = accepted.clone();
    let binding = next
        .authorization()
        .and_then(|authorization| authorization.secret_binding.clone())
        .ok_or_else(|| "public-v1 authorization signing lacks a Secret binding".to_string())?;
    if binding.key_id != material.key_id()
        || binding.verification_key != material.verification_key()
    {
        return Err("public-v1 authorization signing key differs from accepted binding".into());
    }
    let intent = find_active_intent_mut(next.authorization_mut(), operation_id)
        .ok_or_else(|| "public-v1 authorization signing names no active intent".to_string())?;
    if intent.stage != PublicV1AuthorizationIntentStage::Prepared {
        return Err("public-v1 authorization signing is out of order".into());
    }
    let previous = intent.clone();
    let claims = intent
        .claims
        .as_ref()
        .ok_or_else(|| "public-v1 authorization signing lacks typed claims".to_string())?;
    let envelope = match (claims.build.as_deref(), claims.peer.as_deref()) {
        (Some(claims), None) => PublicV1AuthorizationEnvelope::build(
            material
                .signing_key()
                .sign_build_authorization(claims.clone())
                .map_err(|_| "failed to sign public-v1 build authorization".to_string())?,
        ),
        (None, Some(claims)) => PublicV1AuthorizationEnvelope::peer(
            material
                .signing_key()
                .sign_peer_session_authorization(claims.clone())
                .map_err(|_| "failed to sign public-v1 peer authorization".to_string())?,
        ),
        _ => return Err("public-v1 authorization signing lacks typed claims".into()),
    };
    intent.signature = Some(envelope.signature().map_err(str::to_owned)?);
    intent.audit_digest = Some(envelope.audit_digest().map_err(str::to_owned)?);
    intent.envelope = Some(envelope);
    intent.stage = PublicV1AuthorizationIntentStage::Signed;
    intent.validate(Some(&binding)).map_err(str::to_owned)?;
    if !intent.is_successor_of(&previous) {
        return Err("public-v1 authorization signing transition is not monotonic".into());
    }
    next.validate_authorization().map_err(str::to_owned)?;
    Ok(next)
}

pub(crate) fn dispatch_public_v1_authorization(
    accepted: &PublicV1AcceptedStatus,
    operation_id: &OperationId,
) -> Result<PublicV1AuthorizationDispatch, String> {
    let intent = accepted
        .authorization()
        .and_then(|authorization| {
            active_intents(authorization).find(|intent| &intent.operation_id == operation_id)
        })
        .ok_or_else(|| "public-v1 authorization dispatch names no active intent".to_string())?;
    if intent.stage != PublicV1AuthorizationIntentStage::Signed {
        return Err("public-v1 authorization dispatch is out of order".into());
    }
    let dispatch = PublicV1AuthorizationDispatch {
        operation_id: intent.operation_id.clone(),
        revision: intent.revision,
        kind: intent.kind,
        envelope: intent
            .envelope
            .clone()
            .ok_or_else(|| "signed public-v1 authorization lacks its envelope".to_string())?,
        audit_digest: intent
            .audit_digest
            .ok_or_else(|| "signed public-v1 authorization lacks its audit digest".to_string())?,
    };
    dispatch.validate()?;
    Ok(dispatch)
}

pub(crate) fn mark_public_v1_authorization_dispatched(
    accepted: &PublicV1AcceptedStatus,
    operation_id: &OperationId,
) -> Result<PublicV1AcceptedStatus, String> {
    update_intent_stage(
        accepted,
        operation_id,
        PublicV1AuthorizationIntentStage::Dispatched,
    )
}
