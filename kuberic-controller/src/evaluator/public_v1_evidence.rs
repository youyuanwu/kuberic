use kuberic_runtime::protocol::public_operations::{
    CanonicalReplicaRebuildRequired, PublicV1AcceptedStatus, PublicV1AgentReport, PublicV1CallKind,
    PublicV1OperationCompletion, PublicV1OperationResult,
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
pub struct PublicV1CompletionRequirement {
    pub consumer: PublicV1EvidenceConsumer,
    pub operation_id: OperationId,
    pub revision: u64,
    pub call_kind: PublicV1CallKind,
    pub history: HistoryContext,
    pub storage_incarnation: ReplicaStorageIncarnationContext,
    pub process_session_id: ProcessSessionId,
    pub previous_configuration_id: Option<ConfigurationId>,
    pub current_configuration_id: Option<ConfigurationId>,
}

impl PublicV1CompletionRequirement {
    pub fn validate(&self) -> Result<(), String> {
        if self.operation_id.is_empty() || self.revision == 0 || self.process_session_id.is_empty()
        {
            return Err("public-v1 completion requirement is incomplete".into());
        }
        self.history.validate().map_err(str::to_owned)?;
        self.storage_incarnation.validate().map_err(str::to_owned)?;
        if !consumer_accepts_call(self.consumer, self.call_kind) {
            return Err("public-v1 consumer cannot accept this call kind".into());
        }
        Ok(())
    }

    fn matches(&self, completion: &PublicV1OperationCompletion) -> bool {
        completion.operation_id == self.operation_id
            && completion.revision == self.revision
            && completion.call_kind == self.call_kind
            && completion.history == self.history
            && completion.storage_incarnation == self.storage_incarnation
            && completion.process_session_id == self.process_session_id
            && completion.previous_configuration_id == self.previous_configuration_id
            && completion.current_configuration_id == self.current_configuration_id
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PublicV1EvidencePlan {
    PersistStatus {
        expected: Box<PublicV1AcceptedStatus>,
        next: Box<PublicV1AcceptedStatus>,
    },
    Accept {
        expected: Box<PublicV1AcceptedStatus>,
        completion: Box<PublicV1OperationCompletion>,
    },
    RetireExactIncarnation {
        expected: Box<PublicV1AcceptedStatus>,
        completion: Box<PublicV1OperationCompletion>,
        storage: Box<ReplicaStorageIncarnationContext>,
    },
    ProvisionReplacement {
        expected: Box<PublicV1AcceptedStatus>,
        completion: Box<PublicV1OperationCompletion>,
        rebuild: Box<CanonicalReplicaRebuildRequired>,
    },
    Wait,
}

pub fn plan_public_v1_evidence(
    accepted: &PublicV1AcceptedStatus,
    report: &PublicV1AgentReport,
    requirement: &PublicV1CompletionRequirement,
) -> Result<PublicV1EvidencePlan, String> {
    accepted.validate_evidence().map_err(str::to_owned)?;
    report.validate().map_err(str::to_owned)?;
    requirement.validate()?;
    if report.identity != accepted.identity
        || report.resource_uid != accepted.history.resource_uid
        || report.history != accepted.history
        || requirement.history != accepted.history
        || report.storage_incarnation != requirement.storage_incarnation
        || report.process_session_id != requirement.process_session_id
    {
        return Err("public-v1 report or requirement differs from accepted authority".into());
    }
    let Some(completion) = report
        .completed_operations
        .iter()
        .find(|completion| completion.operation_id == requirement.operation_id)
    else {
        return Ok(PublicV1EvidencePlan::Wait);
    };
    if !requirement.matches(completion) {
        return Err("public-v1 completion differs from frozen consumer authority".into());
    }
    if let Some(existing) = accepted
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
    let mut next = accepted.clone();
    next.completed_operations.push(
        kuberic_runtime::protocol::public_operations::PublicV1AcceptedOperationCompletion::
            from_completion(completion),
    );
    next.validate_evidence().map_err(str::to_owned)?;
    if !next.is_evidence_successor_of(accepted) {
        return Err("public-v1 completion persistence is not monotonic".into());
    }
    Ok(PublicV1EvidencePlan::PersistStatus {
        expected: Box::new(accepted.clone()),
        next: Box::new(next),
    })
}

fn decide(
    accepted: &PublicV1AcceptedStatus,
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
                rebuild: rebuild.clone(),
            }
        }
        PublicV1OperationResult::Failed(_)
        | PublicV1OperationResult::Ambiguous(_)
        | PublicV1OperationResult::Cancelled => PublicV1EvidencePlan::RetireExactIncarnation {
            expected: Box::new(accepted.clone()),
            completion: Box::new(completion.clone()),
            storage: Box::new(completion.storage_incarnation.clone()),
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
