use kuberic_runtime::protocol::public_operations::{
    CanonicalOperationInputDigest, CanonicalOperationInputDigestV1,
    CanonicalReplicaRebuildRequired, ProviderReplicaIdentity, PublicCatchUpMode,
    PublicOperationReplayDisposition, PublicV1AcceptedOperationCompletion,
    PublicV1AcceptedRebuildKind, PublicV1AcceptedReplayDisposition, PublicV1AcceptedResult,
    PublicV1AcceptedStatus, PublicV1AgentReport, PublicV1CallKind, PublicV1OperationCompletion,
    PublicV1OperationResult,
};
use kuberic_runtime::protocol::types::{
    ConfigurationId, HistoryContext, OperationId, ProcessSessionId,
    ReplicaStorageIncarnationContext,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum PublicV1EvidenceConsumer {
    Topology,
    Failover,
    Switchover,
    RepairReplacement,
    ScaleUp,
    ScaleDown,
    Removal,
    DataLoss,
    SessionRenewal,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PublicV1ResourceAuthority {
    pub storage_incarnation: ReplicaStorageIncarnationContext,
    pub resource_version: String,
}

impl PublicV1ResourceAuthority {
    fn validate(&self) -> Result<(), String> {
        self.storage_incarnation.validate().map_err(str::to_owned)?;
        if self.resource_version.is_empty() {
            return Err("public-v1 resource authority lacks a resource version".into());
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PublicV1AcceptedTransitionIntent {
    pub consumer: PublicV1EvidenceConsumer,
    pub operation_id: OperationId,
    pub revision: u64,
    pub call_kind: PublicV1CallKind,
    pub history: HistoryContext,
    pub source: PublicV1ResourceAuthority,
    pub target: Option<PublicV1ResourceAuthority>,
    pub process_session_id: ProcessSessionId,
    pub input_digest: CanonicalOperationInputDigest,
    pub replay_disposition: PublicOperationReplayDisposition,
    pub previous_configuration_id: Option<ConfigurationId>,
    pub current_configuration_id: Option<ConfigurationId>,
    pub catch_up_mode: Option<PublicCatchUpMode>,
    pub catch_up_specific_quorum: Option<bool>,
}

impl PublicV1AcceptedTransitionIntent {
    pub fn validate(&self) -> Result<(), String> {
        if self.operation_id.is_empty() || self.revision == 0 || self.process_session_id.is_empty()
        {
            return Err("public-v1 accepted transition intent is incomplete".into());
        }
        self.history.validate().map_err(str::to_owned)?;
        self.source.validate()?;
        if let Some(target) = &self.target {
            target.validate()?;
        }
        if !consumer_accepts_call(self.consumer, self.call_kind) {
            return Err("public-v1 consumer cannot accept this call kind".into());
        }
        Ok(())
    }

    pub(crate) fn matches(&self, completion: &PublicV1OperationCompletion) -> bool {
        completion.operation_id == self.operation_id
            && completion.revision == self.revision
            && completion.call_kind == self.call_kind
            && completion.history == self.history
            && completion.storage_incarnation == self.source.storage_incarnation
            && completion.process_session_id == self.process_session_id
            && completion.input_digest == self.input_digest
            && completion.replay_disposition == self.replay_disposition
            && completion.previous_configuration_id == self.previous_configuration_id
            && completion.current_configuration_id == self.current_configuration_id
            && completion.catch_up_mode == self.catch_up_mode
            && completion.catch_up_specific_quorum == self.catch_up_specific_quorum
    }

    pub(crate) fn matches_accepted(
        &self,
        completion: &PublicV1AcceptedOperationCompletion,
    ) -> bool {
        completion.operation_id == self.operation_id
            && completion.revision == self.revision
            && completion.call_kind == self.call_kind
            && completion.history == self.history
            && completion.storage_incarnation == self.source.storage_incarnation
            && completion.process_session_id == self.process_session_id
            && completion.input_digest_version == self.input_digest.version()
            && completion.input_digest_sha256 == self.input_digest.sha256()
            && completion.replay_disposition
                == kuberic_runtime::protocol::public_operations::
                    PublicV1AcceptedReplayDisposition::from(self.replay_disposition)
            && completion.previous_configuration_id == self.previous_configuration_id
            && completion.current_configuration_id == self.current_configuration_id
            && completion.catch_up_mode == self.catch_up_mode
            && completion.catch_up_specific_quorum == self.catch_up_specific_quorum
    }

    pub(crate) fn effect_target(&self) -> &PublicV1ResourceAuthority {
        self.target.as_ref().unwrap_or(&self.source)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PublicV1AcceptedTransitionStatus {
    pub resource_version: String,
    pub evidence: PublicV1AcceptedStatus,
    pub intent: PublicV1AcceptedTransitionIntent,
}

impl PublicV1AcceptedTransitionStatus {
    pub fn validate(&self) -> Result<(), String> {
        if self.resource_version.is_empty() {
            return Err("public-v1 accepted transition status lacks a resource version".into());
        }
        self.evidence.validate_evidence().map_err(str::to_owned)?;
        self.intent.validate()?;
        if self.intent.history != self.evidence.history {
            return Err(
                "public-v1 accepted transition intent differs from accepted history".into(),
            );
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PublicV1EvidencePlan {
    PersistStatus {
        expected: Box<PublicV1AcceptedTransitionStatus>,
        next: Box<PublicV1AcceptedStatus>,
    },
    Accept {
        expected: Box<PublicV1AcceptedTransitionStatus>,
        completion: Box<PublicV1OperationCompletion>,
    },
    RetireExactIncarnation {
        expected: Box<PublicV1AcceptedTransitionStatus>,
        completion: Box<PublicV1OperationCompletion>,
        target: Box<PublicV1ResourceAuthority>,
    },
    ProvisionReplacement {
        expected: Box<PublicV1AcceptedTransitionStatus>,
        completion: Box<PublicV1OperationCompletion>,
        target: Box<PublicV1ResourceAuthority>,
        rebuild: Box<CanonicalReplicaRebuildRequired>,
    },
    Wait,
}

pub fn plan_public_v1_evidence(
    accepted: &PublicV1AcceptedTransitionStatus,
    report: &PublicV1AgentReport,
) -> Result<PublicV1EvidencePlan, String> {
    accepted.validate()?;
    report.validate().map_err(str::to_owned)?;
    let intent = &accepted.intent;
    let completion = report
        .completed_operations
        .iter()
        .find(|completion| completion.operation_id == intent.operation_id);
    if report.identity != accepted.evidence.identity
        || report.resource_uid != accepted.evidence.history.resource_uid
        || report.history != accepted.evidence.history
        || report.history != intent.history
        || report.storage_incarnation != intent.source.storage_incarnation
        || (report.process_session_id != intent.process_session_id && completion.is_none())
    {
        return Err("public-v1 report differs from accepted transition authority".into());
    }
    let Some(completion) = completion else {
        if let Some(pending) = report
            .retry_pending_operations
            .iter()
            .find(|pending| pending.intent.operation_id == intent.operation_id)
            && !crate::cluster_api::public_v1_operations::operation_matches_transition(
                accepted,
                &pending.intent,
            )
        {
            return Err(
                "retry-pending observation differs from frozen accepted transition authority"
                    .into(),
            );
        }
        // A declared quiescent failure remains nonterminal. The operation
        // reconciler explicitly redispatches the frozen command through the
        // ordinary exact-authority executor; it never accepts pending as success.
        return Ok(PublicV1EvidencePlan::Wait);
    };
    if !intent.matches(completion) {
        return Err(
            "public-v1 completion differs from frozen accepted transition authority".into(),
        );
    }
    let accepted_completion = PublicV1AcceptedOperationCompletion::from_completion(completion);
    if let Some(existing) = accepted
        .evidence
        .completed_operations
        .iter()
        .find(|existing| existing.operation_id == accepted_completion.operation_id)
    {
        if existing != &accepted_completion {
            return Err("accepted public-v1 completion changed after persistence".into());
        }
        return Ok(decide(accepted, completion));
    }
    let mut next = accepted.evidence.clone();
    next.completed_operations.push(accepted_completion);
    next.validate_evidence().map_err(str::to_owned)?;
    if !next.is_evidence_successor_of(&accepted.evidence) {
        return Err("public-v1 completion persistence is not monotonic".into());
    }
    Ok(PublicV1EvidencePlan::PersistStatus {
        expected: Box::new(accepted.clone()),
        next: Box::new(next),
    })
}

pub fn plan_persisted_public_v1_evidence(
    accepted: &PublicV1AcceptedTransitionStatus,
) -> Result<Option<PublicV1EvidencePlan>, String> {
    accepted.validate()?;
    let Some(completion) = accepted
        .evidence
        .completed_operations
        .iter()
        .find(|completion| completion.operation_id == accepted.intent.operation_id)
    else {
        return Ok(None);
    };
    if !accepted.intent.matches_accepted(completion) {
        return Err(
            "persisted public-v1 completion differs from accepted transition authority".into(),
        );
    }
    let completion = accepted_completion(completion)?;
    Ok(Some(decide(accepted, &completion)))
}

fn accepted_completion(
    completion: &PublicV1AcceptedOperationCompletion,
) -> Result<PublicV1OperationCompletion, String> {
    let result = match completion.result {
        PublicV1AcceptedResult::Succeeded => PublicV1OperationResult::Succeeded,
        PublicV1AcceptedResult::Failed => PublicV1OperationResult::Failed(
            completion
                .failure_detail
                .clone()
                .ok_or_else(|| "accepted failed completion lacks detail".to_string())?,
        ),
        PublicV1AcceptedResult::Ambiguous => PublicV1OperationResult::Ambiguous(
            completion
                .failure_detail
                .clone()
                .ok_or_else(|| "accepted ambiguous completion lacks detail".to_string())?,
        ),
        PublicV1AcceptedResult::Cancelled => PublicV1OperationResult::Cancelled,
        PublicV1AcceptedResult::RebuildRequired => {
            let rebuild = completion
                .rebuild_required
                .as_ref()
                .ok_or_else(|| "accepted rebuild completion lacks rebuild evidence".to_string())?;
            let rebuild = match rebuild.kind {
                PublicV1AcceptedRebuildKind::LocalProvider => {
                    CanonicalReplicaRebuildRequired::LocalProvider {
                        reason: rebuild.reason,
                        provider: ProviderReplicaIdentity {
                            replica: rebuild.provider_replica.clone().ok_or_else(|| {
                                "accepted local rebuild lacks provider identity".to_string()
                            })?,
                        },
                        operation: rebuild.operation.clone(),
                        local_replica: rebuild.local_replica.clone().ok_or_else(|| {
                            "accepted local rebuild lacks local replica".to_string()
                        })?,
                        local_storage: rebuild.local_storage.clone().ok_or_else(|| {
                            "accepted local rebuild lacks local storage".to_string()
                        })?,
                    }
                }
                PublicV1AcceptedRebuildKind::RemoteBuildTarget => {
                    CanonicalReplicaRebuildRequired::RemoteBuildTarget {
                        reason: rebuild.reason,
                        operation: rebuild.operation.clone(),
                        goal: Box::new(rebuild.goal.clone().ok_or_else(|| {
                            "accepted remote rebuild lacks exact build identity".to_string()
                        })?),
                    }
                }
            };
            PublicV1OperationResult::RebuildRequired(Box::new(rebuild))
        }
    };
    Ok(PublicV1OperationCompletion {
        operation_id: completion.operation_id.clone(),
        revision: completion.revision,
        call_kind: completion.call_kind,
        process_session_id: completion.process_session_id.clone(),
        input_digest: CanonicalOperationInputDigest::V1(CanonicalOperationInputDigestV1 {
            sha256: completion.input_digest_sha256.clone(),
        }),
        replay_disposition: match completion.replay_disposition {
            PublicV1AcceptedReplayDisposition::Convergent => {
                PublicOperationReplayDisposition::Convergent
            }
            PublicV1AcceptedReplayDisposition::RepeatableRead => {
                PublicOperationReplayDisposition::RepeatableRead
            }
            PublicV1AcceptedReplayDisposition::ReevaluateCatchUp => {
                PublicOperationReplayDisposition::ReevaluateCatchUp
            }
            PublicV1AcceptedReplayDisposition::RetainBuildSuccessRetireAmbiguity => {
                PublicOperationReplayDisposition::RetainBuildSuccessRetireAmbiguity
            }
            PublicV1AcceptedReplayDisposition::DataLossConvergent => {
                PublicOperationReplayDisposition::DataLoss(
                    kuberic_runtime::protocol::public_operations::DataLossReplay::Convergent,
                )
            }
            PublicV1AcceptedReplayDisposition::DataLossReplaceOnAmbiguity => {
                PublicOperationReplayDisposition::DataLoss(
                    kuberic_runtime::protocol::public_operations::DataLossReplay::
                        ReplaceOnAmbiguity,
                )
            }
            PublicV1AcceptedReplayDisposition::ExactRemoval => {
                PublicOperationReplayDisposition::ExactRemoval
            }
            PublicV1AcceptedReplayDisposition::Containment => {
                PublicOperationReplayDisposition::Containment
            }
        },
        history: completion.history.clone(),
        storage_incarnation: completion.storage_incarnation.clone(),
        previous_configuration_id: completion.previous_configuration_id.clone(),
        current_configuration_id: completion.current_configuration_id.clone(),
        catch_up_mode: completion.catch_up_mode,
        catch_up_specific_quorum: completion.catch_up_specific_quorum,
        result,
    })
}

fn decide(
    accepted: &PublicV1AcceptedTransitionStatus,
    completion: &PublicV1OperationCompletion,
) -> PublicV1EvidencePlan {
    match &completion.result {
        PublicV1OperationResult::Succeeded
            if completion.call_kind == PublicV1CallKind::Remove
                && accepted.intent.target.is_some() =>
        {
            PublicV1EvidencePlan::RetireExactIncarnation {
                expected: Box::new(accepted.clone()),
                completion: Box::new(completion.clone()),
                target: Box::new(accepted.intent.effect_target().clone()),
            }
        }
        PublicV1OperationResult::Succeeded => PublicV1EvidencePlan::Accept {
            expected: Box::new(accepted.clone()),
            completion: Box::new(completion.clone()),
        },
        PublicV1OperationResult::RebuildRequired(rebuild) => {
            let target = match rebuild.as_ref() {
                CanonicalReplicaRebuildRequired::LocalProvider { .. } => {
                    accepted.intent.source.clone()
                }
                CanonicalReplicaRebuildRequired::RemoteBuildTarget { .. } => {
                    accepted.intent.effect_target().clone()
                }
            };
            PublicV1EvidencePlan::ProvisionReplacement {
                expected: Box::new(accepted.clone()),
                completion: Box::new(completion.clone()),
                target: Box::new(target),
                rebuild: rebuild.clone(),
            }
        }
        PublicV1OperationResult::Failed(_)
        | PublicV1OperationResult::Ambiguous(_)
        | PublicV1OperationResult::Cancelled => PublicV1EvidencePlan::RetireExactIncarnation {
            expected: Box::new(accepted.clone()),
            completion: Box::new(completion.clone()),
            target: Box::new(accepted.intent.effect_target().clone()),
        },
    }
}

fn consumer_accepts_call(consumer: PublicV1EvidenceConsumer, call: PublicV1CallKind) -> bool {
    match consumer {
        PublicV1EvidenceConsumer::Topology => matches!(
            call,
            PublicV1CallKind::Open
                | PublicV1CallKind::Role
                | PublicV1CallKind::Epoch
                | PublicV1CallKind::Configuration
                | PublicV1CallKind::CatchUp
                | PublicV1CallKind::Progress
                | PublicV1CallKind::Remove
        ),
        PublicV1EvidenceConsumer::Failover => matches!(
            call,
            PublicV1CallKind::InitialPrimary | PublicV1CallKind::FailoverPromotion
        ),
        PublicV1EvidenceConsumer::Switchover => call == PublicV1CallKind::Swap,
        PublicV1EvidenceConsumer::RepairReplacement => call == PublicV1CallKind::Build,
        PublicV1EvidenceConsumer::ScaleUp => {
            matches!(
                call,
                PublicV1CallKind::Build | PublicV1CallKind::Configuration
            )
        }
        PublicV1EvidenceConsumer::ScaleDown => {
            matches!(
                call,
                PublicV1CallKind::Remove | PublicV1CallKind::Configuration
            )
        }
        PublicV1EvidenceConsumer::Removal => matches!(
            call,
            PublicV1CallKind::Remove | PublicV1CallKind::Close | PublicV1CallKind::Abort
        ),
        PublicV1EvidenceConsumer::DataLoss => matches!(
            call,
            PublicV1CallKind::InitialPrimary | PublicV1CallKind::FailoverPromotion
        ),
        PublicV1EvidenceConsumer::SessionRenewal => matches!(
            call,
            PublicV1CallKind::Role | PublicV1CallKind::Epoch | PublicV1CallKind::Configuration
        ),
    }
}
