//! Dormant next-protocol public-operation admission and task ownership.

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::sync::Arc;

use tokio::sync::{Mutex, watch};
use tokio::task::JoinHandle;

use crate::host::state::{PublicOperationDisposition, PublicOperationRecord, PublicOperationStage};
use crate::host::store::{AgentStore, BeginPublicOperation};
use crate::host::{HostError, Result};
use crate::protocol::public_operations::{
    PublicOperationClass, PublicOperationIntent, PublicOperationPreviewIdentity,
};
use crate::protocol::types::{OperationId, ProcessSessionId};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CallbackContainment {
    RootTask,
    ObjectOwned,
}

pub(crate) struct PartitionOperation {
    store: Arc<dyn AgentStore>,
    record: watch::Sender<PublicOperationRecord>,
    task: Mutex<Option<JoinHandle<()>>>,
    containment: Mutex<CallbackContainment>,
}

impl PartitionOperation {
    fn new(store: Arc<dyn AgentStore>, record: PublicOperationRecord) -> Self {
        let (record, _) = watch::channel(record);
        Self {
            store,
            record,
            task: Mutex::new(None),
            containment: Mutex::new(CallbackContainment::RootTask),
        }
    }

    pub(crate) fn intent(&self) -> PublicOperationIntent {
        self.record.borrow().intent.clone()
    }

    pub(crate) fn snapshot(&self) -> PublicOperationRecord {
        self.record.borrow().clone()
    }

    pub(crate) async fn spawn_root<F, E>(
        self: &Arc<Self>,
        containment: CallbackContainment,
        future: F,
    ) -> Result<()>
    where
        F: Future<Output = std::result::Result<(), E>> + Send + 'static,
        E: std::fmt::Display + Send + 'static,
    {
        let current = self.snapshot();
        if current.stage != PublicOperationStage::Ready {
            return Err(HostError::CommandRejected(format!(
                "public operation {} is not ready",
                current.intent.operation_id
            )));
        }
        *self.containment.lock().await = containment;
        let running = self
            .store
            .advance_public_operation(
                &current.intent.operation_id,
                PublicOperationStage::Ready,
                PublicOperationStage::Running,
                None,
            )
            .await?;
        self.record.send_replace(running);

        let operation = Arc::clone(self);
        let task = tokio::spawn(async move {
            let disposition = match future.await {
                Ok(()) => PublicOperationDisposition::Succeeded,
                Err(error) => PublicOperationDisposition::Failed(error.to_string()),
            };
            let operation_id = operation.intent().operation_id;
            if let Ok(completed) = operation
                .store
                .advance_public_operation(
                    &operation_id,
                    PublicOperationStage::Running,
                    PublicOperationStage::Completed,
                    Some(disposition),
                )
                .await
            {
                operation.record.send_replace(completed);
            }
        });
        *self.task.lock().await = Some(task);
        Ok(())
    }

    pub(crate) async fn cancel_root(&self) -> Result<PublicOperationRecord> {
        let current = self.snapshot();
        if current.stage == PublicOperationStage::Completed
            || current.stage == PublicOperationStage::ContainmentPending
        {
            return Ok(current);
        }

        if let Some(task) = self.task.lock().await.take() {
            task.abort();
            match task.await {
                Ok(()) => {}
                Err(error) if error.is_cancelled() => {}
                Err(error) => {
                    return Err(HostError::CommandRejected(format!(
                        "public-operation task join failed: {error}"
                    )));
                }
            }
        }

        let current = self.snapshot();
        if current.stage == PublicOperationStage::Completed {
            return Ok(current);
        }
        let containment = *self.containment.lock().await;
        let (next, disposition) = match containment {
            CallbackContainment::RootTask => (
                PublicOperationStage::Completed,
                PublicOperationDisposition::Cancelled,
            ),
            CallbackContainment::ObjectOwned => (
                PublicOperationStage::ContainmentPending,
                PublicOperationDisposition::Cancelled,
            ),
        };
        let updated = self
            .store
            .advance_public_operation(
                &current.intent.operation_id,
                current.stage,
                next,
                Some(disposition),
            )
            .await?;
        self.record.send_replace(updated.clone());
        Ok(updated)
    }

    pub(crate) async fn complete_containment(&self) -> Result<PublicOperationRecord> {
        let current = self.snapshot();
        if current.stage == PublicOperationStage::Completed {
            return Ok(current);
        }
        if current.stage != PublicOperationStage::ContainmentPending {
            return Err(HostError::CommandRejected(format!(
                "public operation {} has no pending containment",
                current.intent.operation_id
            )));
        }
        let completed = self
            .store
            .advance_public_operation(
                &current.intent.operation_id,
                PublicOperationStage::ContainmentPending,
                PublicOperationStage::Completed,
                Some(PublicOperationDisposition::Contained),
            )
            .await?;
        self.record.send_replace(completed.clone());
        Ok(completed)
    }

    pub(crate) async fn wait_for_terminal(&self) -> PublicOperationRecord {
        let mut receiver = self.record.subscribe();
        loop {
            let record = receiver.borrow().clone();
            if matches!(
                record.stage,
                PublicOperationStage::ContainmentPending | PublicOperationStage::Completed
            ) {
                return record;
            }
            if receiver.changed().await.is_err() {
                return record;
            }
        }
    }

    async fn activate(&self) -> Result<()> {
        let current = self.snapshot();
        if current.stage != PublicOperationStage::WaitingForContainment {
            return Ok(());
        }
        let ready = self
            .store
            .advance_public_operation(
                &current.intent.operation_id,
                PublicOperationStage::WaitingForContainment,
                PublicOperationStage::Ready,
                None,
            )
            .await?;
        self.record.send_replace(ready);
        Ok(())
    }
}

#[derive(Default)]
struct RegistryState {
    operations: BTreeMap<OperationId, Arc<PartitionOperation>>,
}

pub(crate) struct PartitionOperationRegistry {
    store: Arc<dyn AgentStore>,
    preview: PublicOperationPreviewIdentity,
    process_session_id: ProcessSessionId,
    admission: Mutex<()>,
    state: Mutex<RegistryState>,
    revision: watch::Sender<u64>,
}

impl PartitionOperationRegistry {
    pub(crate) fn new(
        store: Arc<dyn AgentStore>,
        preview: PublicOperationPreviewIdentity,
        process_session_id: ProcessSessionId,
    ) -> Result<Arc<Self>> {
        if !preview.is_valid() {
            return Err(HostError::CommandRejected(
                "invalid public-operation preview identity".into(),
            ));
        }
        if process_session_id.is_empty() {
            return Err(HostError::CommandRejected(
                "public-operation process session is empty".into(),
            ));
        }
        let (revision, _) = watch::channel(0);
        Ok(Arc::new(Self {
            store,
            preview,
            process_session_id,
            admission: Mutex::new(()),
            state: Mutex::new(RegistryState::default()),
            revision,
        }))
    }

    pub(crate) fn revision_receiver(&self) -> watch::Receiver<u64> {
        self.revision.subscribe()
    }

    pub(crate) async fn admit(
        self: &Arc<Self>,
        intent: PublicOperationIntent,
    ) -> Result<Arc<PartitionOperation>> {
        intent
            .validate()
            .map_err(|message| HostError::CommandRejected(message.to_string()))?;
        if intent.preview != self.preview || intent.process_session_id != self.process_session_id {
            return Err(HostError::IdentityMismatch(
                "public-operation preview or process session changed".into(),
            ));
        }

        let _admission = self.admission.lock().await;
        if let Some(existing) = self
            .state
            .lock()
            .await
            .operations
            .get(&intent.operation_id)
            .cloned()
        {
            if existing.intent() == intent {
                return Ok(existing);
            }
            return Err(HostError::DurableEffectConflict(
                "public-operation ID was reused with changed input".into(),
            ));
        }

        let existing = self
            .state
            .lock()
            .await
            .operations
            .values()
            .filter(|operation| operation.snapshot().stage != PublicOperationStage::Completed)
            .cloned()
            .collect::<Vec<_>>();
        let mut blockers = BTreeSet::new();
        let mut superseded = Vec::new();
        for operation in existing {
            match relation(&operation.intent(), &intent) {
                AdmissionRelation::Coexist => {}
                AdmissionRelation::Supersede => {
                    blockers.insert(operation.intent().operation_id.clone());
                    superseded.push(operation);
                }
                AdmissionRelation::Reject(reason) => {
                    return Err(HostError::CommandRejected(reason.into()));
                }
            }
        }
        let blocker_ids = blockers.iter().cloned().collect::<Vec<_>>();
        let record = match self
            .store
            .begin_public_operation(&intent, &blocker_ids)
            .await?
        {
            BeginPublicOperation::Ready(record)
            | BeginPublicOperation::Waiting(record)
            | BeginPublicOperation::Pending(record)
            | BeginPublicOperation::Completed(record) => record,
        };
        let operation = Arc::new(PartitionOperation::new(self.store.clone(), record));
        self.state
            .lock()
            .await
            .operations
            .insert(intent.operation_id.clone(), operation.clone());
        self.bump_revision();
        drop(_admission);

        for displaced in superseded {
            displaced.cancel_root().await?;
        }
        self.refresh_waiters().await?;
        Ok(operation)
    }

    pub(crate) async fn complete_containment(
        &self,
        operation_id: &OperationId,
    ) -> Result<PublicOperationRecord> {
        let operation = self
            .state
            .lock()
            .await
            .operations
            .get(operation_id)
            .cloned()
            .ok_or_else(|| {
                HostError::CommandRejected(format!(
                    "public operation {operation_id} is not registered"
                ))
            })?;
        let completed = operation.complete_containment().await?;
        self.bump_revision();
        self.refresh_waiters().await?;
        Ok(completed)
    }

    pub(crate) async fn recover_unowned(&self) -> Result<Vec<PublicOperationRecord>> {
        let records = self.store.public_operation_records().await?;
        let mut recovered = Vec::new();
        for record in records {
            if self
                .state
                .lock()
                .await
                .operations
                .contains_key(&record.intent.operation_id)
            {
                continue;
            }
            let operation = Arc::new(PartitionOperation::new(self.store.clone(), record.clone()));
            self.state
                .lock()
                .await
                .operations
                .insert(record.intent.operation_id.clone(), operation.clone());
            let record = if record.stage == PublicOperationStage::Completed {
                record
            } else {
                let detail = if record.intent.process_session_id == self.process_session_id {
                    "public operation recovered without its owned task"
                } else {
                    "public operation belongs to a predecessor process session"
                };
                let updated = self
                    .store
                    .advance_public_operation(
                        &record.intent.operation_id,
                        record.stage,
                        PublicOperationStage::ContainmentPending,
                        Some(PublicOperationDisposition::Ambiguous(detail.into())),
                    )
                    .await?;
                operation.record.send_replace(updated.clone());
                updated
            };
            recovered.push(record);
        }
        if !recovered.is_empty() {
            self.bump_revision();
        }
        Ok(recovered)
    }

    pub(crate) async fn operation(
        &self,
        operation_id: &OperationId,
    ) -> Option<Arc<PartitionOperation>> {
        self.state
            .lock()
            .await
            .operations
            .get(operation_id)
            .cloned()
    }

    async fn refresh_waiters(&self) -> Result<()> {
        let operations = self
            .state
            .lock()
            .await
            .operations
            .values()
            .cloned()
            .collect::<Vec<_>>();
        let completed = operations
            .iter()
            .filter(|operation| operation.snapshot().stage == PublicOperationStage::Completed)
            .map(|operation| operation.intent().operation_id)
            .collect::<BTreeSet<_>>();
        for operation in operations {
            let record = operation.snapshot();
            if record.stage == PublicOperationStage::WaitingForContainment
                && record
                    .blockers
                    .iter()
                    .all(|blocker| completed.contains(blocker))
            {
                operation.activate().await?;
                self.bump_revision();
            }
        }
        Ok(())
    }

    fn bump_revision(&self) {
        let next = self.revision.borrow().wrapping_add(1);
        self.revision.send_replace(next);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AdmissionRelation {
    Coexist,
    Supersede,
    Reject(&'static str),
}

fn relation(
    existing: &PublicOperationIntent,
    incoming: &PublicOperationIntent,
) -> AdmissionRelation {
    use AdmissionRelation::{Coexist, Reject, Supersede};
    use PublicOperationClass::{
        Abort, Authority, Build, Close, PermanentFault, PlannedSwap, Remove, TransientFault,
    };

    match (&existing.class, &incoming.class) {
        (PermanentFault, _) => Reject("permanent fault is already terminal"),
        (_, PermanentFault) => Supersede,
        (Abort, _) => Reject("abort containment is already terminal"),
        (_, Abort) => Supersede,
        (Close, Close) => Reject("a different close operation is already active"),
        (Close, TransientFault) => Supersede,
        (Close, _) => Reject("close containment is already active"),
        (TransientFault, Close) => Reject("transient fault containment is already active"),
        (TransientFault, TransientFault) => {
            Reject("a different transient fault operation is already active")
        }
        (_, TransientFault) => Supersede,
        (TransientFault, _) => Reject("transient fault containment is already active"),
        (_, Close) => Supersede,
        (Build { target: left }, Build { target: right }) if left != right => Coexist,
        (Build { target: left }, Remove { target: right }) if left == right => Supersede,
        (Build { .. }, Remove { .. }) | (Remove { .. }, Build { .. }) => Coexist,
        (Remove { target: left }, Remove { target: right }) if left != right => Coexist,
        (Build { .. }, Authority | PlannedSwap) => Supersede,
        (Authority, Build { .. }) if existing.revision == incoming.revision => Coexist,
        (Authority, Build { .. }) => {
            Reject("build authority revision does not match current authority")
        }
        (PlannedSwap, Build { .. }) => Reject("planned swap is exclusive with builds"),
        (Authority | PlannedSwap, Authority | PlannedSwap) => {
            if incoming.revision > existing.revision {
                Supersede
            } else {
                Reject("authority revision is not newer")
            }
        }
        (Remove { .. }, Authority | PlannedSwap) => Supersede,
        (Authority | PlannedSwap, Remove { .. }) => Coexist,
        (Build { .. }, Build { .. }) | (Remove { .. }, Remove { .. }) => {
            Reject("same-target operation is already active")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::types::ReplicaId;

    fn intent(class: PublicOperationClass, revision: u64) -> PublicOperationIntent {
        PublicOperationIntent {
            preview: PublicOperationPreviewIdentity::new(1),
            operation_id: OperationId::new(format!("operation-{revision}-{class:?}")),
            revision,
            process_session_id: ProcessSessionId::new("session"),
            class,
            input_digest: format!("digest-{revision}"),
        }
    }

    #[test]
    fn admission_relation_covers_the_reviewed_compatibility_edges() {
        use AdmissionRelation::{Coexist, Reject, Supersede};
        use PublicOperationClass::{
            Abort, Authority, Build, Close, PermanentFault, PlannedSwap, Remove, TransientFault,
        };

        let cases = [
            (Authority, 1, Authority, 2, Supersede),
            (
                Authority,
                2,
                Authority,
                1,
                Reject("authority revision is not newer"),
            ),
            (
                Build {
                    target: ReplicaId::new(2),
                },
                1,
                Build {
                    target: ReplicaId::new(3),
                },
                1,
                Coexist,
            ),
            (
                Build {
                    target: ReplicaId::new(2),
                },
                1,
                Build {
                    target: ReplicaId::new(2),
                },
                1,
                Reject("same-target operation is already active"),
            ),
            (
                Build {
                    target: ReplicaId::new(2),
                },
                1,
                Remove {
                    target: ReplicaId::new(2),
                },
                1,
                Supersede,
            ),
            (
                Build {
                    target: ReplicaId::new(2),
                },
                1,
                Remove {
                    target: ReplicaId::new(3),
                },
                1,
                Coexist,
            ),
            (
                Authority,
                1,
                Build {
                    target: ReplicaId::new(2),
                },
                1,
                Coexist,
            ),
            (
                Authority,
                1,
                Build {
                    target: ReplicaId::new(2),
                },
                2,
                Reject("build authority revision does not match current authority"),
            ),
            (
                PlannedSwap,
                1,
                Build {
                    target: ReplicaId::new(2),
                },
                1,
                Reject("planned swap is exclusive with builds"),
            ),
            (
                Build {
                    target: ReplicaId::new(2),
                },
                1,
                PlannedSwap,
                2,
                Supersede,
            ),
            (Authority, 1, Close, 1, Supersede),
            (Close, 1, Abort, 1, Supersede),
            (
                Abort,
                1,
                Close,
                1,
                Reject("abort containment is already terminal"),
            ),
            (Close, 1, TransientFault, 1, Supersede),
            (
                TransientFault,
                1,
                Close,
                1,
                Reject("transient fault containment is already active"),
            ),
            (TransientFault, 1, PermanentFault, 1, Supersede),
            (
                PermanentFault,
                1,
                Abort,
                1,
                Reject("permanent fault is already terminal"),
            ),
        ];

        for (existing, existing_revision, incoming, incoming_revision, expected) in cases {
            assert_eq!(
                relation(
                    &intent(existing, existing_revision),
                    &intent(incoming, incoming_revision),
                ),
                expected
            );
        }
    }
}
