//! Dormant next-protocol public-operation admission and task ownership.

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::sync::Arc;

use tokio::sync::{Mutex, oneshot, watch};
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
    /// Joining the root future proves all operation-owned work terminated.
    RootTask,
    /// Successful callback completion is the public contract boundary; failure or
    /// cancellation still requires object/process containment.
    ObjectOwnedOnInterruption,
}

pub(crate) struct PartitionOperation {
    store: Arc<dyn AgentStore>,
    record: watch::Sender<PublicOperationRecord>,
    failure: watch::Sender<Option<String>>,
    task: Mutex<Option<JoinHandle<Result<()>>>>,
    lifecycle: Mutex<()>,
    containment: Mutex<CallbackContainment>,
    coordination_active: std::sync::atomic::AtomicBool,
    needs_recovery: std::sync::atomic::AtomicBool,
    shutting_down: Arc<std::sync::atomic::AtomicBool>,
    revision: watch::Sender<u64>,
    descendants: Mutex<Option<watch::Receiver<bool>>>,
    #[cfg(all(test, feature = "testing"))]
    cut: Mutex<Option<Arc<crate::host::public_lifecycle::PublicOperationCut>>>,
}

impl PartitionOperation {
    fn new(
        store: Arc<dyn AgentStore>,
        record: PublicOperationRecord,
        revision: watch::Sender<u64>,
        needs_recovery: bool,
        shutting_down: Arc<std::sync::atomic::AtomicBool>,
    ) -> Self {
        let (record, _) = watch::channel(record);
        let (failure, _) = watch::channel(None);
        Self {
            store,
            record,
            failure,
            task: Mutex::new(None),
            lifecycle: Mutex::new(()),
            containment: Mutex::new(CallbackContainment::RootTask),
            coordination_active: std::sync::atomic::AtomicBool::new(false),
            needs_recovery: std::sync::atomic::AtomicBool::new(needs_recovery),
            shutting_down,
            revision,
            descendants: Mutex::new(None),
            #[cfg(all(test, feature = "testing"))]
            cut: Mutex::new(None),
        }
    }

    pub(crate) fn intent(&self) -> PublicOperationIntent {
        self.record.borrow().intent.clone()
    }

    pub(crate) fn snapshot(&self) -> PublicOperationRecord {
        self.record.borrow().clone()
    }

    #[cfg(all(test, feature = "testing"))]
    pub(crate) async fn track_cut(
        &self,
        cut: Option<Arc<crate::host::public_lifecycle::PublicOperationCut>>,
    ) {
        *self.cut.lock().await = cut;
    }

    #[cfg(all(test, feature = "testing"))]
    pub(crate) async fn crash_root(&self) {
        if let Some(task) = self.task.lock().await.take() {
            task.abort();
            let _ = task.await;
        }
        self.needs_recovery
            .store(true, std::sync::atomic::Ordering::Release);
    }

    pub(crate) async fn track_containment(&self, witness: watch::Receiver<bool>) -> Result<()> {
        let _lifecycle = self.lifecycle.lock().await;
        if self.snapshot().stage != PublicOperationStage::Ready {
            return Err(HostError::CommandRejected(
                "containment must be bound before dispatch".into(),
            ));
        }
        *self.descendants.lock().await = Some(witness);
        Ok(())
    }

    async fn descendants_contained(&self) -> bool {
        self.descendants
            .lock()
            .await
            .as_ref()
            .is_none_or(|witness| *witness.borrow())
    }

    async fn settle_witness(&self) -> Result<()> {
        let witness = self.descendants.lock().await.clone();
        if witness.is_some_and(|witness| *witness.borrow())
            && self.snapshot().stage == PublicOperationStage::ContainmentPending
        {
            self.complete_containment().await?;
        }
        Ok(())
    }

    pub(crate) async fn wait_until_ready(&self) -> Result<bool> {
        let mut record = self.record.subscribe();
        loop {
            match record.borrow().stage {
                PublicOperationStage::Ready => return Ok(true),
                PublicOperationStage::WaitingForContainment => {}
                _ => return Ok(false),
            }
            record
                .changed()
                .await
                .map_err(|_| HostError::CommandRejected("operation owner lost".into()))?;
        }
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
        let _lifecycle = self.lifecycle.lock().await;
        if self
            .shutting_down
            .load(std::sync::atomic::Ordering::Acquire)
        {
            return Err(HostError::CommandRejected(
                "public-operation registry is shutting down".into(),
            ));
        }
        let current = self.snapshot();
        if current.stage != PublicOperationStage::Ready {
            return Err(HostError::CommandRejected(format!(
                "public operation {} is not ready",
                current.intent.operation_id
            )));
        }
        if self.task.lock().await.is_some() {
            return Err(HostError::CommandRejected(format!(
                "public operation {} already owns a root task",
                current.intent.operation_id
            )));
        }
        *self.containment.lock().await = containment;

        let (start, started) = oneshot::channel();
        let operation = Arc::clone(self);
        let task = tokio::spawn(async move {
            if started.await.is_err() {
                return Ok(());
            }
            let result = future.await.map_err(|error| error.to_string());
            let recorded = operation.record_root_result(result, containment).await;
            if let Err(error) = &recorded {
                operation.record_failure(error);
            }
            recorded
        });
        *self.task.lock().await = Some(task);

        let running = match self
            .store
            .advance_public_operation(
                &current.intent.operation_id,
                current.intent.revision,
                &current.intent.process_session_id,
                PublicOperationStage::Ready,
                PublicOperationStage::Running,
                None,
            )
            .await
        {
            Ok(running) => running,
            Err(error) => {
                if let Some(task) = self.task.lock().await.take() {
                    task.abort();
                    let _ = task.await;
                }
                return Err(error);
            }
        };
        self.record.send_replace(running);
        self.bump_revision();
        let _ = start.send(());
        Ok(())
    }

    async fn record_root_result(
        &self,
        result: std::result::Result<(), String>,
        containment: CallbackContainment,
    ) -> Result<()> {
        let _lifecycle = self.lifecycle.lock().await;
        let current = self.snapshot();
        if current.stage != PublicOperationStage::Running {
            return Err(HostError::StaleEffectCompletion(format!(
                "public operation {} returned at {:?}",
                current.intent.operation_id, current.stage
            )));
        }
        let disposition = match result {
            Ok(()) => PublicOperationDisposition::Succeeded,
            Err(error) => PublicOperationDisposition::Failed(error),
        };

        if (matches!(disposition, PublicOperationDisposition::Failed(_))
            && containment == CallbackContainment::ObjectOwnedOnInterruption)
            || !self.descendants_contained().await
        {
            let pending = self
                .advance(
                    PublicOperationStage::Running,
                    PublicOperationStage::ContainmentPending,
                    Some(disposition),
                )
                .await?;
            self.record.send_replace(pending);
            self.bump_revision();
            return Ok(());
        }

        let applied = self
            .advance(
                PublicOperationStage::Running,
                PublicOperationStage::CallbackApplied,
                Some(disposition),
            )
            .await?;
        self.record.send_replace(applied);
        self.bump_revision();
        #[cfg(all(test, feature = "testing"))]
        if let Some(cut) = self.cut.lock().await.clone() {
            cut.pause(
                crate::host::public_lifecycle::PublicCutPosition::CallbackApplied,
                0,
            )
            .await;
        }
        let completed = self
            .advance(
                PublicOperationStage::CallbackApplied,
                PublicOperationStage::Completed,
                None,
            )
            .await?;
        self.record.send_replace(completed);
        self.bump_revision();
        #[cfg(all(test, feature = "testing"))]
        if let Some(cut) = self.cut.lock().await.clone() {
            cut.pause(
                crate::host::public_lifecycle::PublicCutPosition::Completed,
                0,
            )
            .await;
        }
        Ok(())
    }

    pub(crate) async fn cancel_root(&self) -> Result<PublicOperationRecord> {
        let task = {
            let _lifecycle = self.lifecycle.lock().await;
            let current = self.snapshot();
            if matches!(
                current.stage,
                PublicOperationStage::Completed | PublicOperationStage::ContainmentPending
            ) {
                return Ok(current);
            }
            let task = self.task.lock().await.take();
            if let Some(task) = &task {
                task.abort();
            } else {
                return self.persist_cancellation(current, None).await;
            }
            task
        };
        let mut task_failure = None;
        if let Some(task) = task {
            match task.await {
                Ok(Ok(())) => {}
                Ok(Err(error)) => {
                    task_failure = Some(error.to_string());
                }
                Err(error) if error.is_cancelled() => {}
                Err(error) => {
                    task_failure = Some(format!("public-operation task join failed: {error}"));
                }
            }
        }

        let _lifecycle = self.lifecycle.lock().await;
        let current = self.snapshot();
        if matches!(
            current.stage,
            PublicOperationStage::Completed | PublicOperationStage::ContainmentPending
        ) {
            return Ok(current);
        }
        if current.stage == PublicOperationStage::CallbackApplied {
            if current.superseded_by.is_some() {
                let pending = self
                    .advance(
                        PublicOperationStage::CallbackApplied,
                        PublicOperationStage::ContainmentPending,
                        Some(PublicOperationDisposition::Ambiguous(
                            "callback was applied before supersession".into(),
                        )),
                    )
                    .await?;
                self.record.send_replace(pending.clone());
                self.bump_revision();
                return Ok(pending);
            }
            let completed = self
                .advance(
                    PublicOperationStage::CallbackApplied,
                    PublicOperationStage::Completed,
                    None,
                )
                .await?;
            self.record.send_replace(completed.clone());
            self.bump_revision();
            return Ok(completed);
        }

        self.persist_cancellation(current, task_failure).await
    }

    async fn persist_cancellation(
        &self,
        current: PublicOperationRecord,
        task_failure: Option<String>,
    ) -> Result<PublicOperationRecord> {
        let containment = *self.containment.lock().await;
        let next = match containment {
            CallbackContainment::RootTask if self.descendants_contained().await => {
                PublicOperationStage::Completed
            }
            CallbackContainment::RootTask => PublicOperationStage::ContainmentPending,
            CallbackContainment::ObjectOwnedOnInterruption => {
                PublicOperationStage::ContainmentPending
            }
        };
        let disposition = task_failure.map_or(PublicOperationDisposition::Cancelled, |error| {
            PublicOperationDisposition::Ambiguous(error)
        });
        let updated = match self.advance(current.stage, next, Some(disposition)).await {
            Ok(updated) => updated,
            Err(error) => {
                self.record_failure(&error);
                return Err(error);
            }
        };
        self.record.send_replace(updated.clone());
        self.bump_revision();
        Ok(updated)
    }

    pub(crate) async fn complete_containment(&self) -> Result<PublicOperationRecord> {
        let _lifecycle = self.lifecycle.lock().await;
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
        if !self.descendants_contained().await {
            return Err(HostError::CommandRejected(
                "root termination is not descendant containment".into(),
            ));
        }
        let completed = match self
            .advance(
                PublicOperationStage::ContainmentPending,
                PublicOperationStage::Completed,
                None,
            )
            .await
        {
            Ok(completed) => completed,
            Err(error) => {
                self.record_failure(&error);
                return Err(error);
            }
        };
        self.record.send_replace(completed.clone());
        self.bump_revision();
        Ok(completed)
    }

    pub(crate) async fn wait_for_terminal(&self) -> Result<PublicOperationRecord> {
        let mut record = self.record.subscribe();
        let mut failure = self.failure.subscribe();
        loop {
            let current = record.borrow().clone();
            if matches!(
                current.stage,
                PublicOperationStage::ContainmentPending | PublicOperationStage::Completed
            ) {
                return Ok(current);
            }
            if let Some(message) = failure.borrow().clone() {
                return Err(HostError::StaleEffectCompletion(message));
            }
            tokio::select! {
                changed = record.changed() => {
                    if changed.is_err() {
                        return Ok(current);
                    }
                }
                changed = failure.changed() => {
                    if changed.is_err() {
                        return Ok(current);
                    }
                }
            }
        }
    }

    async fn activate(&self) -> Result<()> {
        let _lifecycle = self.lifecycle.lock().await;
        let current = self.snapshot();
        if current.stage != PublicOperationStage::WaitingForContainment {
            return Ok(());
        }
        let ready = match self
            .advance(
                PublicOperationStage::WaitingForContainment,
                PublicOperationStage::Ready,
                None,
            )
            .await
        {
            Ok(ready) => ready,
            Err(error) => {
                self.record_failure(&error);
                return Err(error);
            }
        };
        self.record.send_replace(ready);
        self.bump_revision();
        Ok(())
    }

    async fn complete_attachment(&self) -> Result<()> {
        let _lifecycle = self.lifecycle.lock().await;
        let current = self.snapshot();
        if current.stage != PublicOperationStage::WaitingForContainment
            || !matches!(
                current.disposition,
                Some(PublicOperationDisposition::Attached(_))
            )
        {
            return Ok(());
        }
        let completed = self
            .advance(
                PublicOperationStage::WaitingForContainment,
                PublicOperationStage::Completed,
                None,
            )
            .await?;
        self.record.send_replace(completed);
        self.bump_revision();
        Ok(())
    }

    async fn advance(
        &self,
        expected: PublicOperationStage,
        next: PublicOperationStage,
        disposition: Option<PublicOperationDisposition>,
    ) -> Result<PublicOperationRecord> {
        let intent = self.intent();
        self.store
            .advance_public_operation(
                &intent.operation_id,
                intent.revision,
                &intent.process_session_id,
                expected,
                next,
                disposition,
            )
            .await
    }

    async fn has_live_work(&self) -> bool {
        self.coordination_active
            .load(std::sync::atomic::Ordering::Acquire)
            || self
                .task
                .lock()
                .await
                .as_ref()
                .is_some_and(|task| !task.is_finished())
    }

    async fn recover_if_needed(
        &self,
        current_session: &ProcessSessionId,
    ) -> Result<Option<PublicOperationRecord>> {
        let _lifecycle = self.lifecycle.lock().await;
        if !self.needs_recovery() || self.has_live_work().await {
            return Ok(None);
        }
        let operation_id = self.intent().operation_id;
        let record = self
            .store
            .public_operation_records()
            .await?
            .into_iter()
            .find(|record| record.intent.operation_id == operation_id)
            .ok_or_else(|| {
                HostError::CommandRejected(format!(
                    "public operation {operation_id} disappeared during recovery"
                ))
            })?;
        self.record.send_replace(record.clone());
        if matches!(
            record.stage,
            PublicOperationStage::Completed | PublicOperationStage::ContainmentPending
        ) {
            self.recovery_complete();
            return Ok(None);
        }
        if matches!(
            record.disposition,
            Some(PublicOperationDisposition::Attached(_))
        ) {
            self.recovery_complete();
            return Ok(None);
        }

        let same_session = &record.intent.process_session_id == current_session;
        let recovered = if same_session
            && record.superseded_by.is_none()
            && crate::host::public_lifecycle::replayable(&record)
            && matches!(
                record.stage,
                PublicOperationStage::Ready | PublicOperationStage::Running
            ) {
            if record.stage == PublicOperationStage::Ready {
                record
            } else {
                self.advance(record.stage, PublicOperationStage::Ready, None)
                    .await?
            }
        } else if record.stage == PublicOperationStage::CallbackApplied && same_session {
            if record.superseded_by.is_some() {
                self.advance(
                    PublicOperationStage::CallbackApplied,
                    PublicOperationStage::ContainmentPending,
                    Some(PublicOperationDisposition::Ambiguous(
                        "callback was applied before supersession".into(),
                    )),
                )
                .await?
            } else {
                self.advance(
                    PublicOperationStage::CallbackApplied,
                    PublicOperationStage::Completed,
                    None,
                )
                .await?
            }
        } else {
            let detail = if &record.intent.process_session_id == current_session {
                "public operation recovered without its owned task"
            } else {
                "public operation belongs to a predecessor process session"
            };
            self.advance(
                record.stage,
                PublicOperationStage::ContainmentPending,
                Some(PublicOperationDisposition::Ambiguous(detail.into())),
            )
            .await?
        };
        self.record.send_replace(recovered.clone());
        self.recovery_complete();
        Ok(Some(recovered))
    }

    fn set_coordination_active(&self, active: bool) {
        self.coordination_active
            .store(active, std::sync::atomic::Ordering::Release);
    }

    fn record_failure(&self, error: &HostError) {
        self.needs_recovery
            .store(true, std::sync::atomic::Ordering::Release);
        self.failure.send_replace(Some(error.to_string()));
        self.bump_revision();
    }

    fn needs_recovery(&self) -> bool {
        self.needs_recovery
            .load(std::sync::atomic::Ordering::Acquire)
    }

    fn recovery_complete(&self) {
        self.needs_recovery
            .store(false, std::sync::atomic::Ordering::Release);
    }

    fn mark_superseded_by(&self, successor: &OperationId) -> Result<()> {
        let mut record = self.snapshot();
        if record
            .superseded_by
            .as_ref()
            .is_some_and(|existing| existing != successor)
        {
            return Err(HostError::DurableEffectConflict(format!(
                "public operation {} was superseded by multiple successors",
                record.intent.operation_id
            )));
        }
        if record.superseded_by.as_ref() != Some(successor) {
            record.superseded_by = Some(successor.clone());
            self.record.send_replace(record);
            self.bump_revision();
        }
        Ok(())
    }

    fn bump_revision(&self) {
        let next = self.revision.borrow().wrapping_add(1);
        self.revision.send_replace(next);
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
    coordination: Mutex<Vec<JoinHandle<()>>>,
    revision: watch::Sender<u64>,
    shutting_down: Arc<std::sync::atomic::AtomicBool>,
    terminal_fence: std::sync::atomic::AtomicBool,
    control_tasks: std::sync::Mutex<Vec<JoinHandle<()>>>,
    abort_guard: Arc<std::sync::Mutex<bool>>,
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
            coordination: Mutex::new(Vec::new()),
            revision,
            shutting_down: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            terminal_fence: std::sync::atomic::AtomicBool::new(false),
            control_tasks: std::sync::Mutex::new(Vec::new()),
            abort_guard: Arc::new(std::sync::Mutex::new(false)),
        }))
    }

    pub(crate) fn revision_receiver(&self) -> watch::Receiver<u64> {
        self.revision.subscribe()
    }

    pub(crate) fn fence(&self) {
        self.terminal_fence
            .store(true, std::sync::atomic::Ordering::Release);
    }

    pub(crate) fn abort_guard(&self) -> Arc<std::sync::Mutex<bool>> {
        self.abort_guard.clone()
    }

    pub(crate) fn is_fenced(&self) -> bool {
        self.terminal_fence
            .load(std::sync::atomic::Ordering::Acquire)
    }

    pub(crate) fn own_control_task(&self, future: impl Future<Output = ()> + Send + 'static) {
        self.control_tasks
            .lock()
            .expect("control task lock")
            .push(tokio::spawn(future));
    }

    pub(crate) async fn admit(
        self: &Arc<Self>,
        intent: PublicOperationIntent,
    ) -> Result<Arc<PartitionOperation>> {
        if self
            .shutting_down
            .load(std::sync::atomic::Ordering::Acquire)
        {
            return Err(HostError::CommandRejected(
                "public-operation registry is shutting down".into(),
            ));
        }
        intent
            .validate()
            .map_err(|message| HostError::CommandRejected(message.to_string()))?;
        if self.is_fenced() && !intent.class.is_terminal() {
            return Err(HostError::CommandRejected(
                "preview is synchronously fenced".into(),
            ));
        }
        if intent.preview != self.preview || intent.process_session_id != self.process_session_id {
            return Err(HostError::IdentityMismatch(
                "public-operation preview or process session changed".into(),
            ));
        }

        self.reap_coordination().await;
        let admission = self.admission.lock().await;
        if self
            .shutting_down
            .load(std::sync::atomic::Ordering::Acquire)
        {
            return Err(HostError::CommandRejected(
                "public-operation registry is shutting down".into(),
            ));
        }
        self.load_durable_records().await?;
        if let Some(existing) = self
            .state
            .lock()
            .await
            .operations
            .get(&intent.operation_id)
            .cloned()
        {
            if existing.intent() == intent {
                existing.recover_if_needed(&self.process_session_id).await?;
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
            .cloned()
            .collect::<Vec<_>>();
        if !intent.class.is_terminal()
            && self
                .store
                .load_state()
                .await?
                .public_operation_preview
                .is_some_and(|preview| !preview.history_barriers.is_empty())
        {
            // A rejected successor may contain its predecessor, but never acquires
            // authority or removes the logical history-admission barrier.
            drop(admission);
            for operation in existing {
                if operation.intent().revision < intent.revision
                    && operation.intent().lifecycle.as_ref().is_some_and(|input| {
                        input.possible_data_loss
                            == crate::protocol::public_operations::PossibleDataLossIntent::Possible
                    })
                {
                    operation.cancel_root().await?;
                }
            }
            return Err(HostError::CommandRejected(
                "unresolved history-admission barrier".into(),
            ));
        }
        let highest_retained_authority_revision = existing
            .iter()
            .filter_map(|operation| {
                let record = operation.snapshot();
                (record.stage == PublicOperationStage::Completed
                    && matches!(
                        record.intent.class,
                        PublicOperationClass::Authority | PublicOperationClass::PlannedSwap
                    ))
                .then_some(record.intent.revision)
            })
            .max();
        let mut blockers = BTreeSet::new();
        let mut superseded = Vec::new();
        let mut attached_to = None;
        for operation in existing {
            let record = operation.snapshot();
            if record.superseded_by.is_some()
                || matches!(
                    record.disposition,
                    Some(PublicOperationDisposition::Attached(_))
                )
            {
                continue;
            }
            if record.stage == PublicOperationStage::Completed
                && matches!(
                    record.intent.class,
                    PublicOperationClass::Authority | PublicOperationClass::PlannedSwap
                )
                && highest_retained_authority_revision
                    .is_some_and(|revision| record.intent.revision < revision)
            {
                continue;
            }
            let relation = if record.stage == PublicOperationStage::Completed {
                retained_relation(&record.intent, &intent)
            } else {
                active_relation(&record.intent, &intent)
            };
            match relation {
                AdmissionRelation::Coexist => {}
                AdmissionRelation::Attach => {
                    attached_to.get_or_insert(operation);
                }
                AdmissionRelation::Supersede => {
                    blockers.insert(record.intent.operation_id.clone());
                    blockers.extend(record.blockers.iter().cloned());
                    superseded.push(operation);
                }
                AdmissionRelation::Reject(reason) => {
                    return Err(HostError::CommandRejected(reason.into()));
                }
            }
        }
        if let Some(owner) = attached_to {
            let attached = self
                .record_attachment(intent, owner.intent().operation_id)
                .await?;
            drop(admission);
            return Ok(attached);
        }

        let blocker_ids = blockers.iter().cloned().collect::<Vec<_>>();
        let superseded_ids = superseded
            .iter()
            .map(|operation| operation.intent().operation_id)
            .collect::<Vec<_>>();
        let record = match self
            .store
            .begin_public_operation(&intent, &blocker_ids, &superseded_ids)
            .await?
        {
            BeginPublicOperation::Ready(record)
            | BeginPublicOperation::Waiting(record)
            | BeginPublicOperation::Pending(record)
            | BeginPublicOperation::Completed(record) => record,
        };
        for displaced in &superseded {
            displaced.mark_superseded_by(&intent.operation_id)?;
        }
        let operation = Arc::new(PartitionOperation::new(
            self.store.clone(),
            record,
            self.revision.clone(),
            false,
            self.shutting_down.clone(),
        ));
        self.state
            .lock()
            .await
            .operations
            .insert(intent.operation_id.clone(), operation.clone());
        self.bump_revision();

        if !superseded.is_empty() {
            operation.set_coordination_active(true);
            let registry = Arc::clone(self);
            let admitted = operation.clone();
            let task = tokio::spawn(async move {
                let result = registry
                    .finish_supersession(admitted.clone(), superseded)
                    .await;
                admitted.set_coordination_active(false);
                if let Err(error) = result {
                    admitted.record_failure(&error);
                    let current = admitted.snapshot();
                    if matches!(
                        current.stage,
                        PublicOperationStage::WaitingForContainment | PublicOperationStage::Ready
                    ) && let Ok(pending) = admitted
                        .advance(
                            current.stage,
                            PublicOperationStage::ContainmentPending,
                            Some(PublicOperationDisposition::Ambiguous(error.to_string())),
                        )
                        .await
                    {
                        admitted.record.send_replace(pending);
                    }
                }
            });
            self.coordination.lock().await.push(task);
        }
        drop(admission);
        Ok(operation)
    }

    pub(crate) async fn report_fault(
        self: &Arc<Self>,
        intent: PublicOperationIntent,
    ) -> Result<Arc<PartitionOperation>> {
        if !matches!(
            intent.class,
            PublicOperationClass::TransientFault | PublicOperationClass::PermanentFault
        ) || intent.lifecycle.is_some()
            || intent.program.is_some()
        {
            return Err(HostError::CommandRejected(
                "fault reporting requires an exact fault-only public operation".into(),
            ));
        }
        let operation = self.admit(intent).await?;
        self.fence();
        let dispatch = {
            let operation = operation.clone();
            async move {
                if operation.wait_until_ready().await? {
                    operation
                        .spawn_root(CallbackContainment::RootTask, async {
                            Ok::<(), HostError>(())
                        })
                        .await?;
                }
                Ok::<(), HostError>(())
            }
        };
        if operation.snapshot().stage == PublicOperationStage::Ready {
            dispatch.await?;
        } else if operation.snapshot().stage == PublicOperationStage::WaitingForContainment {
            self.own_control_task(async move {
                if let Err(error) = dispatch.await {
                    tracing::warn!(%error, "preview fault containment remains fenced");
                }
            });
        }
        Ok(operation)
    }

    async fn record_attachment(
        &self,
        intent: PublicOperationIntent,
        owner: OperationId,
    ) -> Result<Arc<PartitionOperation>> {
        let record = self.store.attach_public_operation(&intent, &owner).await?;
        let operation = Arc::new(PartitionOperation::new(
            self.store.clone(),
            record,
            self.revision.clone(),
            false,
            self.shutting_down.clone(),
        ));
        self.state
            .lock()
            .await
            .operations
            .insert(intent.operation_id.clone(), operation.clone());
        self.bump_revision();
        Ok(operation)
    }

    async fn finish_supersession(
        &self,
        _admitted: Arc<PartitionOperation>,
        superseded: Vec<Arc<PartitionOperation>>,
    ) -> Result<()> {
        let mut first_error = None;
        for displaced in superseded {
            if let Err(error) = displaced.cancel_root().await {
                first_error.get_or_insert(error);
            } else if let Err(error) = displaced.settle_witness().await {
                first_error.get_or_insert(error);
            }
        }
        if let Err(error) = self.refresh_waiters().await {
            first_error.get_or_insert(error);
        }
        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
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
        self.refresh_waiters().await?;
        Ok(completed)
    }

    pub(crate) async fn recover_unowned(&self) -> Result<Vec<PublicOperationRecord>> {
        let _admission = self.admission.lock().await;
        self.load_durable_records().await?;
        let operations = self
            .state
            .lock()
            .await
            .operations
            .values()
            .cloned()
            .collect::<Vec<_>>();
        let mut recovered = Vec::new();
        for operation in operations {
            operation.settle_witness().await?;
            if let Some(record) = operation
                .recover_if_needed(&self.process_session_id)
                .await?
            {
                recovered.push(record);
            }
        }
        if !recovered.is_empty() {
            self.bump_revision();
        }
        self.refresh_waiters().await?;
        Ok(recovered)
    }

    pub(crate) async fn shutdown(&self) -> Result<()> {
        self.shutting_down
            .store(true, std::sync::atomic::Ordering::Release);
        let controls = std::mem::take(&mut *self.control_tasks.lock().expect("control task lock"));
        for task in controls {
            task.abort();
            let _ = task.await;
        }
        let admission = self.admission.lock().await;
        let mut first_error = None;
        let coordination = std::mem::take(&mut *self.coordination.lock().await);
        for task in coordination {
            if let Err(error) = task.await {
                first_error.get_or_insert_with(|| {
                    HostError::CommandRejected(format!(
                        "public-operation coordination join failed: {error}"
                    ))
                });
            }
        }
        let operations = self
            .state
            .lock()
            .await
            .operations
            .values()
            .cloned()
            .collect::<Vec<_>>();
        for operation in operations {
            if !matches!(
                operation.snapshot().stage,
                PublicOperationStage::Completed | PublicOperationStage::ContainmentPending
            ) && let Err(error) = operation.cancel_root().await
            {
                first_error.get_or_insert(error);
            }
        }
        drop(admission);
        if let Err(error) = self.recover_unowned().await {
            first_error.get_or_insert(error);
        }
        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
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

    async fn load_durable_records(&self) -> Result<()> {
        let records = self.store.public_operation_records().await?;
        let mut state = self.state.lock().await;
        for record in records {
            state
                .operations
                .entry(record.intent.operation_id.clone())
                .or_insert_with(|| {
                    Arc::new(PartitionOperation::new(
                        self.store.clone(),
                        record,
                        self.revision.clone(),
                        true,
                        self.shutting_down.clone(),
                    ))
                });
        }
        Ok(())
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
                if matches!(
                    record.disposition,
                    Some(PublicOperationDisposition::Attached(_))
                ) {
                    operation.complete_attachment().await?;
                } else {
                    operation.activate().await?;
                }
            }
        }
        Ok(())
    }

    async fn reap_coordination(&self) {
        self.coordination
            .lock()
            .await
            .retain(|task| !task.is_finished());
    }

    fn bump_revision(&self) {
        let next = self.revision.borrow().wrapping_add(1);
        self.revision.send_replace(next);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AdmissionRelation {
    Coexist,
    Attach,
    Supersede,
    Reject(&'static str),
}

fn active_relation(
    existing: &PublicOperationIntent,
    incoming: &PublicOperationIntent,
) -> AdmissionRelation {
    use AdmissionRelation::{Attach, Coexist, Reject, Supersede};
    use PublicOperationClass::{
        Abort, Authority, Build, Close, DropReplacement, PermanentFault, PlannedSwap, Remove,
        Restart, TransientFault,
    };

    match (&existing.class, &incoming.class) {
        (PermanentFault, class) if class.is_terminal() => Attach,
        (PermanentFault, _) => Reject("permanent fault is already terminal"),
        (Restart | DropReplacement, class) if class.is_terminal() => Attach,
        (Restart | DropReplacement, _) => Reject("restart or drop action is already active"),
        (Abort, class) if class.is_terminal() => Attach,
        (Abort, _) => Reject("abort containment is already terminal"),
        (_, PermanentFault) => Supersede,
        (_, Restart | DropReplacement) => Supersede,
        (TransientFault, Close | Abort | TransientFault) => Attach,
        (_, Abort) => Supersede,
        (Close, Close) => Attach,
        (Close, TransientFault) => Supersede,
        (Close, _) => Reject("close containment is already active"),
        (_, TransientFault) => Supersede,
        (TransientFault, _) => Reject("transient fault containment is already active"),
        (_, Close) => Supersede,
        (Build { target: left }, Build { target: right })
            if left != right && existing.revision == incoming.revision =>
        {
            Coexist
        }
        (Build { target: left }, Build { target: right }) if left != right => {
            Reject("build revisions do not match")
        }
        (Build { target: left }, Remove { target: right })
            if left == right && existing.revision == incoming.revision =>
        {
            Supersede
        }
        (Build { target: left }, Remove { target: right }) if left == right => {
            Reject("same-target removal revision does not match build")
        }
        (Remove { target: left }, Build { target: right }) if left == right => {
            Reject("same-target removal is already active")
        }
        (Build { .. }, Remove { .. }) | (Remove { .. }, Build { .. })
            if existing.revision == incoming.revision =>
        {
            Coexist
        }
        (Build { .. }, Remove { .. }) | (Remove { .. }, Build { .. }) => {
            Reject("build and removal revisions do not match")
        }
        (Remove { target: left }, Remove { target: right })
            if left != right && existing.revision == incoming.revision =>
        {
            Coexist
        }
        (Remove { target: left }, Remove { target: right }) if left != right => {
            Reject("removal revisions do not match")
        }
        (Build { .. }, Authority | PlannedSwap) if incoming.revision > existing.revision => {
            Supersede
        }
        (Build { .. }, Authority | PlannedSwap) => {
            Reject("authority revision does not exceed active build")
        }
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
        (Remove { .. }, Authority | PlannedSwap) if incoming.revision > existing.revision => {
            Supersede
        }
        (Remove { .. }, Authority | PlannedSwap) => {
            Reject("authority revision does not exceed active removal")
        }
        (Authority | PlannedSwap, Remove { .. }) if existing.revision == incoming.revision => {
            Coexist
        }
        (Authority | PlannedSwap, Remove { .. }) => {
            Reject("removal authority revision does not match current authority")
        }
        (Build { .. }, Build { .. }) | (Remove { .. }, Remove { .. }) => {
            Reject("same-target operation is already active")
        }
    }
}

fn retained_relation(
    existing: &PublicOperationIntent,
    incoming: &PublicOperationIntent,
) -> AdmissionRelation {
    use AdmissionRelation::{Attach, Coexist, Reject, Supersede};
    use PublicOperationClass::{
        Abort, Authority, Build, Close, DropReplacement, PermanentFault, PlannedSwap, Remove,
        Restart, TransientFault,
    };

    match (&existing.class, &incoming.class) {
        (Close | Abort | PermanentFault | Restart | DropReplacement, class)
            if class.is_terminal() =>
        {
            Attach
        }
        (Close | Abort | PermanentFault | Restart | DropReplacement, _) => {
            Reject("replica is durably terminal")
        }
        (TransientFault, PermanentFault) => Supersede,
        (TransientFault, class) if class.is_terminal() => Attach,
        (TransientFault, _) => Reject("transient fault requires lifecycle control"),
        (Authority | PlannedSwap, Authority | PlannedSwap)
            if incoming.revision <= existing.revision =>
        {
            Reject("authority revision does not exceed retained authority")
        }
        (Authority | PlannedSwap, Build { .. } | Remove { .. })
            if existing.revision != incoming.revision =>
        {
            Reject("operation revision does not match retained authority")
        }
        _ => Coexist,
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
            lifecycle: None,
            program: None,
        }
    }

    #[test]
    fn admission_relation_covers_the_reviewed_compatibility_edges() {
        use AdmissionRelation::{Attach, Coexist, Reject, Supersede};
        use PublicOperationClass::{
            Abort, Authority, Build, Close, DropReplacement, PermanentFault, PlannedSwap, Remove,
            Restart, TransientFault,
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
                    target: ReplicaId::new(3),
                },
                2,
                Reject("build revisions do not match"),
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
                    target: ReplicaId::new(2),
                },
                2,
                Reject("same-target removal revision does not match build"),
            ),
            (
                Remove {
                    target: ReplicaId::new(2),
                },
                1,
                Build {
                    target: ReplicaId::new(2),
                },
                1,
                Reject("same-target removal is already active"),
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
            (Authority, 1, Close, 1, Supersede),
            (Close, 1, Close, 1, Attach),
            (Close, 1, Abort, 1, Supersede),
            (Abort, 1, Close, 1, Attach),
            (Close, 1, TransientFault, 1, Supersede),
            (TransientFault, 1, Close, 1, Attach),
            (TransientFault, 1, PermanentFault, 1, Supersede),
            (PermanentFault, 1, Abort, 1, Attach),
            (Authority, 1, Restart, 1, Supersede),
            (Restart, 1, Close, 1, Attach),
            (Authority, 1, DropReplacement, 1, Supersede),
            (DropReplacement, 1, Abort, 1, Attach),
        ];

        for (existing, existing_revision, incoming, incoming_revision, expected) in cases {
            assert_eq!(
                active_relation(
                    &intent(existing, existing_revision),
                    &intent(incoming, incoming_revision),
                ),
                expected
            );
        }
    }

    #[test]
    fn retained_terminal_and_authority_records_fence_new_admission() {
        use AdmissionRelation::{Attach, Coexist, Reject};
        use PublicOperationClass::{Abort, Authority, Build, Close, PermanentFault};

        assert_eq!(
            retained_relation(&intent(Authority, 4), &intent(Authority, 4)),
            Reject("authority revision does not exceed retained authority")
        );
        assert_eq!(
            retained_relation(&intent(Authority, 4), &intent(Authority, 5)),
            Coexist
        );
        assert_eq!(
            retained_relation(
                &intent(Authority, 4),
                &intent(
                    Build {
                        target: ReplicaId::new(2),
                    },
                    5,
                ),
            ),
            Reject("operation revision does not match retained authority")
        );
        assert_eq!(
            retained_relation(&intent(Abort, 1), &intent(Close, 2)),
            Attach
        );
        assert_eq!(
            retained_relation(&intent(PermanentFault, 1), &intent(Authority, 2)),
            Reject("replica is durably terminal")
        );
    }

    #[test]
    fn terminal_pair_matrices_are_exhaustive() {
        use AdmissionRelation::{Attach, Supersede};
        use PublicOperationClass::{
            Abort, Close, DropReplacement, PermanentFault, Restart, TransientFault,
        };

        let classes = [
            Close,
            Abort,
            TransientFault,
            PermanentFault,
            Restart,
            DropReplacement,
        ];
        let active = [
            [
                Attach, Supersede, Supersede, Supersede, Supersede, Supersede,
            ],
            [Attach, Attach, Attach, Attach, Attach, Attach],
            [Attach, Attach, Attach, Supersede, Supersede, Supersede],
            [Attach, Attach, Attach, Attach, Attach, Attach],
            [Attach, Attach, Attach, Attach, Attach, Attach],
            [Attach, Attach, Attach, Attach, Attach, Attach],
        ];
        let retained = [
            [Attach, Attach, Attach, Attach, Attach, Attach],
            [Attach, Attach, Attach, Attach, Attach, Attach],
            [Attach, Attach, Attach, Supersede, Attach, Attach],
            [Attach, Attach, Attach, Attach, Attach, Attach],
            [Attach, Attach, Attach, Attach, Attach, Attach],
            [Attach, Attach, Attach, Attach, Attach, Attach],
        ];

        for (existing_index, existing) in classes.iter().cloned().enumerate() {
            for (incoming_index, incoming) in classes.iter().cloned().enumerate() {
                assert_eq!(
                    active_relation(&intent(existing.clone(), 1), &intent(incoming.clone(), 2)),
                    active[existing_index][incoming_index],
                    "active terminal pair {existing:?} -> {incoming:?}"
                );
                assert_eq!(
                    retained_relation(&intent(existing.clone(), 1), &intent(incoming.clone(), 2)),
                    retained[existing_index][incoming_index],
                    "retained terminal pair {existing:?} -> {incoming:?}"
                );
            }
        }
    }
}
