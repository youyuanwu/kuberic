use crate::evaluator::public_v1_evidence::PublicV1EvidenceConsumer;
use kuberic_runtime::protocol::public_operations::{
    CanonicalReplicaRebuildRequired, PublicV1AcceptedStatus, PublicV1OperationCompletion,
};
use kuberic_runtime::protocol::types::{OperationId, ReplicaStorageIncarnationContext};
use std::sync::Mutex;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PublicV1EvidenceEffect {
    Accepted {
        consumer: PublicV1EvidenceConsumer,
        operation_id: OperationId,
    },
    Retired {
        consumer: PublicV1EvidenceConsumer,
        operation_id: OperationId,
        storage: Box<ReplicaStorageIncarnationContext>,
    },
    Replacement {
        consumer: PublicV1EvidenceConsumer,
        operation_id: OperationId,
        rebuild: Box<CanonicalReplicaRebuildRequired>,
    },
}

struct PublicV1EvidenceModel {
    status: Option<PublicV1AcceptedStatus>,
    effects: Vec<PublicV1EvidenceEffect>,
    retired: Vec<ReplicaStorageIncarnationContext>,
    replacements: Vec<CanonicalReplicaRebuildRequired>,
}

pub struct PublicV1EvidenceState {
    model: Mutex<PublicV1EvidenceModel>,
}

impl PublicV1EvidenceState {
    pub fn new(status: Option<PublicV1AcceptedStatus>) -> Self {
        Self {
            model: Mutex::new(PublicV1EvidenceModel {
                status,
                effects: Vec::new(),
                retired: Vec::new(),
                replacements: Vec::new(),
            }),
        }
    }

    pub fn status(&self) -> Option<PublicV1AcceptedStatus> {
        self.model.lock().unwrap().status.clone()
    }

    pub fn effects(&self) -> Vec<PublicV1EvidenceEffect> {
        self.model.lock().unwrap().effects.clone()
    }

    pub fn retired_incarnations(&self) -> Vec<ReplicaStorageIncarnationContext> {
        self.model.lock().unwrap().retired.clone()
    }

    pub fn replacements(&self) -> Vec<CanonicalReplicaRebuildRequired> {
        self.model.lock().unwrap().replacements.clone()
    }

    fn require_current(
        model: &PublicV1EvidenceModel,
        expected: &PublicV1AcceptedStatus,
    ) -> Result<(), String> {
        if model.status.as_ref() != Some(expected) {
            return Err("stale public-v1 evidence model authority".into());
        }
        Ok(())
    }
}

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
        consumer: PublicV1EvidenceConsumer,
        completion: &PublicV1OperationCompletion,
    ) -> Result<(), String>;

    async fn retire_public_v1_incarnation(
        &self,
        expected: &PublicV1AcceptedStatus,
        consumer: PublicV1EvidenceConsumer,
        completion: &PublicV1OperationCompletion,
        storage: &ReplicaStorageIncarnationContext,
    ) -> Result<(), String>;

    async fn provision_public_v1_replacement(
        &self,
        expected: &PublicV1AcceptedStatus,
        consumer: PublicV1EvidenceConsumer,
        completion: &PublicV1OperationCompletion,
        rebuild: &CanonicalReplicaRebuildRequired,
    ) -> Result<(), String>;
}

#[async_trait::async_trait]
impl PublicV1EvidenceApi for PublicV1EvidenceState {
    async fn observe_public_v1_evidence_status(
        &self,
    ) -> Result<Option<PublicV1AcceptedStatus>, String> {
        Ok(self.status())
    }

    async fn persist_public_v1_evidence_status(
        &self,
        expected: &PublicV1AcceptedStatus,
        next: &PublicV1AcceptedStatus,
    ) -> Result<(), String> {
        let mut model = self.model.lock().unwrap();
        Self::require_current(&model, expected)?;
        model.status = Some(next.clone());
        Ok(())
    }

    async fn accept_public_v1_completion(
        &self,
        expected: &PublicV1AcceptedStatus,
        consumer: PublicV1EvidenceConsumer,
        completion: &PublicV1OperationCompletion,
    ) -> Result<(), String> {
        let mut model = self.model.lock().unwrap();
        Self::require_current(&model, expected)?;
        model.effects.push(PublicV1EvidenceEffect::Accepted {
            consumer,
            operation_id: completion.operation_id.clone(),
        });
        Ok(())
    }

    async fn retire_public_v1_incarnation(
        &self,
        expected: &PublicV1AcceptedStatus,
        consumer: PublicV1EvidenceConsumer,
        completion: &PublicV1OperationCompletion,
        storage: &ReplicaStorageIncarnationContext,
    ) -> Result<(), String> {
        let mut model = self.model.lock().unwrap();
        Self::require_current(&model, expected)?;
        model.retired.push(storage.clone());
        model.effects.push(PublicV1EvidenceEffect::Retired {
            consumer,
            operation_id: completion.operation_id.clone(),
            storage: Box::new(storage.clone()),
        });
        Ok(())
    }

    async fn provision_public_v1_replacement(
        &self,
        expected: &PublicV1AcceptedStatus,
        consumer: PublicV1EvidenceConsumer,
        completion: &PublicV1OperationCompletion,
        rebuild: &CanonicalReplicaRebuildRequired,
    ) -> Result<(), String> {
        let mut model = self.model.lock().unwrap();
        Self::require_current(&model, expected)?;
        model.replacements.push(rebuild.clone());
        model.effects.push(PublicV1EvidenceEffect::Replacement {
            consumer,
            operation_id: completion.operation_id.clone(),
            rebuild: Box::new(rebuild.clone()),
        });
        Ok(())
    }
}
