use crate::evaluator::public_v1_evidence::{
    PublicV1AcceptedTransitionIntent, PublicV1AcceptedTransitionStatus, PublicV1EvidenceConsumer,
    PublicV1ResourceAuthority,
};
use kuberic_runtime::protocol::public_operations::{
    CanonicalReplicaRebuildRequired, PublicV1AcceptedStatus, PublicV1OperationCompletion,
    PublicV1OperationResult,
};
use kuberic_runtime::protocol::types::{
    HistoryContext, OperationId, ReplicaStorageIncarnationContext,
};
use std::sync::Mutex;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PublicV1ResourceDisposition {
    Active,
    Retired,
    ReplacementRequested,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublicV1ResourceProof {
    pub history: HistoryContext,
    pub storage_incarnation: ReplicaStorageIncarnationContext,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublicV1EvidenceResource {
    pub storage_incarnation: ReplicaStorageIncarnationContext,
    pub resource_version: String,
    pub proof: Option<PublicV1ResourceProof>,
    pub disposition: PublicV1ResourceDisposition,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PublicV1EvidenceEffect {
    Accepted {
        consumer: PublicV1EvidenceConsumer,
        operation_id: OperationId,
        revision: u64,
    },
    Retired {
        consumer: PublicV1EvidenceConsumer,
        operation_id: OperationId,
        revision: u64,
        storage: Box<ReplicaStorageIncarnationContext>,
    },
    Replacement {
        consumer: PublicV1EvidenceConsumer,
        operation_id: OperationId,
        revision: u64,
        storage: Box<ReplicaStorageIncarnationContext>,
        rebuild: Box<CanonicalReplicaRebuildRequired>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PublicV1EffectAction {
    Accept,
    Retire(PublicV1ResourceAuthority),
    Replacement {
        target: PublicV1ResourceAuthority,
        rebuild: Box<CanonicalReplicaRebuildRequired>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PublicV1EffectKey {
    intent: PublicV1AcceptedTransitionIntent,
    completion: PublicV1OperationCompletion,
    action: PublicV1EffectAction,
}

struct PublicV1EvidenceModel {
    transition: PublicV1AcceptedTransitionStatus,
    resources: Vec<PublicV1EvidenceResource>,
    effects: Vec<PublicV1EvidenceEffect>,
    effect_keys: Vec<PublicV1EffectKey>,
    status_revision: u64,
}

pub struct PublicV1EvidenceState {
    model: Mutex<PublicV1EvidenceModel>,
}

impl PublicV1EvidenceState {
    pub fn new(
        transition: PublicV1AcceptedTransitionStatus,
        resources: Vec<PublicV1EvidenceResource>,
    ) -> Result<Self, String> {
        transition.validate()?;
        for resource in &resources {
            resource
                .storage_incarnation
                .validate()
                .map_err(str::to_owned)?;
            if resource.resource_version.is_empty() {
                return Err("public-v1 evidence resource lacks a resource version".into());
            }
        }
        for (index, resource) in resources.iter().enumerate() {
            if resources[..index]
                .iter()
                .any(|existing| existing.storage_incarnation == resource.storage_incarnation)
            {
                return Err("public-v1 evidence resources contain a duplicate incarnation".into());
            }
        }
        Ok(Self {
            model: Mutex::new(PublicV1EvidenceModel {
                transition,
                resources,
                effects: Vec::new(),
                effect_keys: Vec::new(),
                status_revision: 0,
            }),
        })
    }

    pub fn transition(&self) -> PublicV1AcceptedTransitionStatus {
        self.model.lock().unwrap().transition.clone()
    }

    pub fn effects(&self) -> Vec<PublicV1EvidenceEffect> {
        self.model.lock().unwrap().effects.clone()
    }

    pub fn resources(&self) -> Vec<PublicV1EvidenceResource> {
        self.model.lock().unwrap().resources.clone()
    }

    fn require_current(
        model: &PublicV1EvidenceModel,
        expected: &PublicV1AcceptedTransitionStatus,
    ) -> Result<(), String> {
        if &model.transition != expected {
            return Err("stale public-v1 accepted transition authority".into());
        }
        Ok(())
    }

    fn require_resource<'a>(
        model: &'a mut PublicV1EvidenceModel,
        expected: &PublicV1AcceptedTransitionStatus,
        authority: &PublicV1ResourceAuthority,
    ) -> Result<&'a mut PublicV1EvidenceResource, String> {
        let resource = model
            .resources
            .iter_mut()
            .find(|resource| resource.storage_incarnation == authority.storage_incarnation)
            .ok_or_else(|| "exact public-v1 evidence resource is absent".to_string())?;
        if resource.resource_version != authority.resource_version {
            return Err("stale public-v1 evidence resource version".into());
        }
        let proof = resource
            .proof
            .as_ref()
            .ok_or_else(|| "public-v1 evidence resource lacks accepted proof".to_string())?;
        if proof.history != expected.evidence.history
            || proof.storage_incarnation != authority.storage_incarnation
        {
            return Err("public-v1 evidence resource proof differs from accepted authority".into());
        }
        Ok(resource)
    }

    fn require_completion(
        expected: &PublicV1AcceptedTransitionStatus,
        completion: &PublicV1OperationCompletion,
    ) -> Result<(), String> {
        if !expected.intent.matches(completion) {
            return Err("public-v1 effect completion differs from accepted intent".into());
        }
        let accepted =
            kuberic_runtime::protocol::public_operations::PublicV1AcceptedOperationCompletion::
                from_completion(completion);
        if !expected
            .evidence
            .completed_operations
            .iter()
            .any(|existing| existing == &accepted)
        {
            return Err("public-v1 effect completion is not persisted accepted evidence".into());
        }
        Ok(())
    }

    fn next_resource_version(resource: &mut PublicV1EvidenceResource) {
        resource.resource_version = format!("{}-next", resource.resource_version);
    }
}

#[async_trait::async_trait]
pub trait PublicV1EvidenceApi: Send + Sync {
    async fn observe_public_v1_evidence_status(
        &self,
    ) -> Result<Option<PublicV1AcceptedTransitionStatus>, String>;

    async fn persist_public_v1_evidence_status(
        &self,
        expected: &PublicV1AcceptedTransitionStatus,
        next: &PublicV1AcceptedStatus,
    ) -> Result<(), String>;

    async fn accept_public_v1_completion(
        &self,
        expected: &PublicV1AcceptedTransitionStatus,
        completion: &PublicV1OperationCompletion,
    ) -> Result<(), String>;

    async fn retire_public_v1_incarnation(
        &self,
        expected: &PublicV1AcceptedTransitionStatus,
        completion: &PublicV1OperationCompletion,
        target: &PublicV1ResourceAuthority,
    ) -> Result<(), String>;

    async fn provision_public_v1_replacement(
        &self,
        expected: &PublicV1AcceptedTransitionStatus,
        completion: &PublicV1OperationCompletion,
        target: &PublicV1ResourceAuthority,
        rebuild: &CanonicalReplicaRebuildRequired,
    ) -> Result<(), String>;
}

#[async_trait::async_trait]
impl PublicV1EvidenceApi for PublicV1EvidenceState {
    async fn observe_public_v1_evidence_status(
        &self,
    ) -> Result<Option<PublicV1AcceptedTransitionStatus>, String> {
        Ok(Some(self.transition()))
    }

    async fn persist_public_v1_evidence_status(
        &self,
        expected: &PublicV1AcceptedTransitionStatus,
        next: &PublicV1AcceptedStatus,
    ) -> Result<(), String> {
        let mut model = self.model.lock().unwrap();
        if model.transition.evidence == *next
            && model.transition.intent == expected.intent
            && model.transition.resource_version != expected.resource_version
        {
            return Ok(());
        }
        Self::require_current(&model, expected)?;
        if !next.is_evidence_successor_of(&expected.evidence) {
            return Err("public-v1 evidence persistence is not monotonic".into());
        }
        model.status_revision += 1;
        model.transition.evidence = next.clone();
        model.transition.resource_version = format!("evidence-status-{}", model.status_revision);
        Ok(())
    }

    async fn accept_public_v1_completion(
        &self,
        expected: &PublicV1AcceptedTransitionStatus,
        completion: &PublicV1OperationCompletion,
    ) -> Result<(), String> {
        let mut model = self.model.lock().unwrap();
        Self::require_current(&model, expected)?;
        Self::require_completion(expected, completion)?;
        if completion.result != PublicV1OperationResult::Succeeded {
            return Err("public-v1 acceptance requires a successful completion".into());
        }
        let key = PublicV1EffectKey {
            intent: expected.intent.clone(),
            completion: completion.clone(),
            action: PublicV1EffectAction::Accept,
        };
        if model.effect_keys.contains(&key) {
            return Ok(());
        }
        Self::require_resource(&mut model, expected, &expected.intent.source)?;
        model.effect_keys.push(key);
        model.effects.push(PublicV1EvidenceEffect::Accepted {
            consumer: expected.intent.consumer,
            operation_id: completion.operation_id.clone(),
            revision: completion.revision,
        });
        Ok(())
    }

    async fn retire_public_v1_incarnation(
        &self,
        expected: &PublicV1AcceptedTransitionStatus,
        completion: &PublicV1OperationCompletion,
        target: &PublicV1ResourceAuthority,
    ) -> Result<(), String> {
        let mut model = self.model.lock().unwrap();
        Self::require_current(&model, expected)?;
        Self::require_completion(expected, completion)?;
        if !matches!(
            &completion.result,
            PublicV1OperationResult::Failed(_)
                | PublicV1OperationResult::Ambiguous(_)
                | PublicV1OperationResult::Cancelled
        ) && !(completion.result == PublicV1OperationResult::Succeeded
            && completion.call_kind
                == kuberic_runtime::protocol::public_operations::PublicV1CallKind::Remove
            && expected.intent.target.is_some())
        {
            return Err(
                "public-v1 retirement requires a failed completion or exact successful removal"
                    .into(),
            );
        }
        if target != expected.intent.effect_target() {
            return Err("public-v1 retirement target differs from accepted intent".into());
        }
        let key = PublicV1EffectKey {
            intent: expected.intent.clone(),
            completion: completion.clone(),
            action: PublicV1EffectAction::Retire(target.clone()),
        };
        if model.effect_keys.contains(&key) {
            return Ok(());
        }
        let resource = Self::require_resource(&mut model, expected, target)?;
        resource.disposition = PublicV1ResourceDisposition::Retired;
        Self::next_resource_version(resource);
        model.effect_keys.push(key);
        model.effects.push(PublicV1EvidenceEffect::Retired {
            consumer: expected.intent.consumer,
            operation_id: completion.operation_id.clone(),
            revision: completion.revision,
            storage: Box::new(target.storage_incarnation.clone()),
        });
        Ok(())
    }

    async fn provision_public_v1_replacement(
        &self,
        expected: &PublicV1AcceptedTransitionStatus,
        completion: &PublicV1OperationCompletion,
        target: &PublicV1ResourceAuthority,
        rebuild: &CanonicalReplicaRebuildRequired,
    ) -> Result<(), String> {
        let mut model = self.model.lock().unwrap();
        Self::require_current(&model, expected)?;
        Self::require_completion(expected, completion)?;
        if !matches!(
            &completion.result,
            PublicV1OperationResult::RebuildRequired(expected_rebuild)
                if expected_rebuild.as_ref() == rebuild
        ) {
            return Err("public-v1 replacement differs from accepted rebuild evidence".into());
        }
        let expected_target = match rebuild {
            CanonicalReplicaRebuildRequired::LocalProvider { .. } => &expected.intent.source,
            CanonicalReplicaRebuildRequired::RemoteBuildTarget { .. } => {
                expected.intent.effect_target()
            }
        };
        if target != expected_target {
            return Err("public-v1 replacement target differs from accepted intent".into());
        }
        let key = PublicV1EffectKey {
            intent: expected.intent.clone(),
            completion: completion.clone(),
            action: PublicV1EffectAction::Replacement {
                target: target.clone(),
                rebuild: Box::new(rebuild.clone()),
            },
        };
        if model.effect_keys.contains(&key) {
            return Ok(());
        }
        let resource = Self::require_resource(&mut model, expected, target)?;
        resource.disposition = PublicV1ResourceDisposition::ReplacementRequested;
        Self::next_resource_version(resource);
        model.effect_keys.push(key);
        model.effects.push(PublicV1EvidenceEffect::Replacement {
            consumer: expected.intent.consumer,
            operation_id: completion.operation_id.clone(),
            revision: completion.revision,
            storage: Box::new(target.storage_incarnation.clone()),
            rebuild: Box::new(rebuild.clone()),
        });
        Ok(())
    }
}
