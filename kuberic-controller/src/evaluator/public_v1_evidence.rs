use kuberic_runtime::protocol::public_operations::{
    CanonicalOperationInputDigest, CanonicalReplicaRebuildRequired, PublicCatchUpMode,
    PublicOperationReplayDisposition, PublicV1AcceptedStatus, PublicV1AgentReport,
    PublicV1CallKind, PublicV1OperationCompletion, PublicV1OperationResult,
};
use kuberic_runtime::protocol::types::{
    ConfigurationId, HistoryContext, OperationId, ProcessSessionId,
    ReplicaStorageIncarnationContext,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PublicV1EvidenceConsumer {
    Failover,
    Switchover,
    RepairReplacement,
    ScaleUp,
    ScaleDown,
    Removal,
    DataLoss,
    SessionRenewal,
}

#[derive(Debug, Clone, PartialEq, Eq)]
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

#[derive(Debug, Clone, PartialEq, Eq)]
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

    pub(crate) fn effect_target(&self) -> &PublicV1ResourceAuthority {
        self.target.as_ref().unwrap_or(&self.source)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
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
    if report.identity != accepted.evidence.identity
        || report.resource_uid != accepted.evidence.history.resource_uid
        || report.history != accepted.evidence.history
        || report.history != intent.history
        || report.storage_incarnation != intent.source.storage_incarnation
        || report.process_session_id != intent.process_session_id
    {
        return Err("public-v1 report differs from accepted transition authority".into());
    }
    let Some(completion) = report
        .completed_operations
        .iter()
        .find(|completion| completion.operation_id == intent.operation_id)
    else {
        return Ok(PublicV1EvidencePlan::Wait);
    };
    if !intent.matches(completion) {
        return Err(
            "public-v1 completion differs from frozen accepted transition authority".into(),
        );
    }
    if let Some(existing) = accepted
        .evidence
        .completed_operations
        .iter()
        .find(|existing| existing.operation_id == completion.operation_id)
    {
        if existing
            != &kuberic_runtime::protocol::public_operations::
                PublicV1AcceptedOperationCompletion::from_completion(completion)
        {
            return Err("accepted public-v1 completion changed after persistence".into());
        }
        return Ok(decide(accepted, completion));
    }
    let mut next = accepted.evidence.clone();
    next.completed_operations.push(
        kuberic_runtime::protocol::public_operations::PublicV1AcceptedOperationCompletion::
            from_completion(completion),
    );
    next.validate_evidence().map_err(str::to_owned)?;
    if !next.is_evidence_successor_of(&accepted.evidence) {
        return Err("public-v1 completion persistence is not monotonic".into());
    }
    Ok(PublicV1EvidencePlan::PersistStatus {
        expected: Box::new(accepted.clone()),
        next: Box::new(next),
    })
}

fn decide(
    accepted: &PublicV1AcceptedTransitionStatus,
    completion: &PublicV1OperationCompletion,
) -> PublicV1EvidencePlan {
    match &completion.result {
        PublicV1OperationResult::Succeeded => PublicV1EvidencePlan::Accept {
            expected: Box::new(accepted.clone()),
            completion: Box::new(completion.clone()),
        },
        PublicV1OperationResult::RebuildRequired(rebuild) => {
            PublicV1EvidencePlan::ProvisionReplacement {
                expected: Box::new(accepted.clone()),
                completion: Box::new(completion.clone()),
                target: Box::new(accepted.intent.effect_target().clone()),
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
        PublicV1EvidenceConsumer::Removal => call == PublicV1CallKind::Remove,
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
