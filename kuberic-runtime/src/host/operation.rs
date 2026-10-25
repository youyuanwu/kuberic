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
    PublicOperationClass, PublicOperationIntent, PublicOperationProgram, PublicSessionRenewalStage,
    PublicV1Identity, PublicV1SessionRenewalStatus,
};
use crate::protocol::types::{OperationId, ProcessSessionId, ReplicaIdentity};

pub(crate) struct PartitionOperation {
    store: Arc<dyn AgentStore>,
    record: watch::Sender<PublicOperationRecord>,
    failure: watch::Sender<Option<String>>,
    task: Mutex<Option<JoinHandle<Result<()>>>>,
    ordinary_join: Mutex<()>,
    lifecycle: Mutex<()>,
    coordination_active: std::sync::atomic::AtomicBool,
    access_invalidated: std::sync::atomic::AtomicBool,
    access_invalidation: std::sync::Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    needs_recovery: std::sync::atomic::AtomicBool,
    shutting_down: Arc<std::sync::atomic::AtomicBool>,
    revision: watch::Sender<u64>,
    descendants: Mutex<Option<watch::Receiver<bool>>>,
    wait_for_descendants: std::sync::atomic::AtomicBool,
    #[cfg(feature = "testing")]
    cut: Mutex<Option<Arc<crate::host::public_lifecycle::PublicOperationCut>>>,
    #[cfg(all(test, feature = "testing"))]
    ordinary_join_entered: std::sync::Mutex<Option<Arc<tokio::sync::Notify>>>,
    #[cfg(all(test, feature = "testing"))]
    ordinary_return_cut: std::sync::Mutex<Option<Arc<RegistryTestCut>>>,
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
            ordinary_join: Mutex::new(()),
            lifecycle: Mutex::new(()),
            coordination_active: std::sync::atomic::AtomicBool::new(false),
            access_invalidated: std::sync::atomic::AtomicBool::new(false),
            access_invalidation: std::sync::Mutex::new(None),
            needs_recovery: std::sync::atomic::AtomicBool::new(needs_recovery),
            shutting_down,
            revision,
            descendants: Mutex::new(None),
            wait_for_descendants: std::sync::atomic::AtomicBool::new(false),
            #[cfg(feature = "testing")]
            cut: Mutex::new(None),
            #[cfg(all(test, feature = "testing"))]
            ordinary_join_entered: Default::default(),
            #[cfg(all(test, feature = "testing"))]
            ordinary_return_cut: Default::default(),
        }
    }

    pub(crate) fn intent(&self) -> PublicOperationIntent {
        self.record.borrow().intent.clone()
    }

    pub(crate) fn snapshot(&self) -> PublicOperationRecord {
        self.record.borrow().clone()
    }

    pub(crate) fn access_live(&self) -> bool {
        !self
            .access_invalidated
            .load(std::sync::atomic::Ordering::Acquire)
            && !self
                .shutting_down
                .load(std::sync::atomic::Ordering::Acquire)
            && self.record.borrow().superseded_by.is_none()
            && !matches!(
                self.record.borrow().disposition,
                Some(
                    PublicOperationDisposition::Failed(_)
                        | PublicOperationDisposition::Cancelled
                        | PublicOperationDisposition::Ambiguous(_)
                        | PublicOperationDisposition::Attached(_)
                )
            )
    }

    pub(crate) fn bind_access_owner(
        &self,
        reserve: impl FnOnce() -> Option<Arc<crate::host::hosting::AccessOwner>>,
    ) -> Option<Arc<crate::host::hosting::AccessOwner>> {
        // Invalidation cannot pass a reservation before its gated owner is bound.
        let mut invalidation = self.access_invalidation.lock().unwrap();
        let owner = reserve()?;
        let invalidate = owner.clone();
        *invalidation = Some(Arc::new(move || invalidate.invalidate()));
        Some(owner)
    }

    fn invalidate_access(&self) {
        let invalidation = self.access_invalidation.lock().unwrap();
        if let Some(invalidate) = invalidation.as_ref() {
            invalidate();
        }
        self.access_invalidated
            .store(true, std::sync::atomic::Ordering::Release);
    }

    #[cfg(feature = "testing")]
    pub(crate) async fn track_cut(
        &self,
        cut: Option<Arc<crate::host::public_lifecycle::PublicOperationCut>>,
    ) {
        *self.cut.lock().await = cut;
    }

    pub(crate) async fn track_containment(
        &self,
        witness: watch::Receiver<bool>,
        wait_for_descendants: bool,
    ) -> Result<()> {
        let _lifecycle = self.lifecycle.lock().await;
        if self.snapshot().stage != PublicOperationStage::Ready {
            return Err(HostError::CommandRejected(
                "containment must be bound before dispatch".into(),
            ));
        }
        *self.descendants.lock().await = Some(witness);
        self.wait_for_descendants
            .store(wait_for_descendants, std::sync::atomic::Ordering::Release);
        Ok(())
    }

    pub(crate) async fn track_recovery_containment(&self, witness: watch::Receiver<bool>) -> bool {
        let _lifecycle = self.lifecycle.lock().await;
        let mut descendants = self.descendants.lock().await;
        if self.snapshot().stage != PublicOperationStage::ContainmentPending
            || descendants.is_some()
        {
            return false;
        }
        *descendants = Some(witness);
        self.wait_for_descendants
            .store(true, std::sync::atomic::Ordering::Release);
        true
    }

    pub(crate) fn record_containment_failure(&self, error: &HostError) {
        tracing::warn!(%error, operation_id = %self.intent().operation_id, "terminal recovery remains uncontained");
        self.failure.send_replace(Some(error.to_string()));
        self.bump_revision();
    }

    async fn descendants_contained(&self) -> bool {
        self.descendants.lock().await.as_ref().map_or(
            !matches!(
                self.intent().program,
                Some(PublicOperationProgram::Close | PublicOperationProgram::Abort)
            ) && !(self.intent().goal().is_some() && self.intent().class.is_terminal()),
            |witness| *witness.borrow(),
        )
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

    async fn wait_for_witness(&self) -> Result<()> {
        let Some(mut witness) = self.descendants.lock().await.clone() else {
            return Ok(());
        };
        while self.snapshot().stage == PublicOperationStage::ContainmentPending
            && !*witness.borrow()
        {
            witness.changed().await.map_err(|_| {
                HostError::CommandRejected(
                    "public-operation containment witness closed before drain".into(),
                )
            })?;
        }
        self.settle_witness().await
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

    pub(crate) async fn spawn_root<F, E>(self: &Arc<Self>, future: F) -> Result<()>
    where
        F: Future<Output = std::result::Result<(), E>> + Send + 'static,
        E: std::fmt::Display + Send + 'static,
    {
        self.spawn_root_with_retry(None, future).await
    }

    pub(crate) async fn spawn_root_with_retry<F, E>(
        self: &Arc<Self>,
        returned_failure: Option<
            Arc<std::sync::Mutex<Option<crate::host::state::PublicCallbackFailure>>>,
        >,
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
        let (start, started) = oneshot::channel();
        let operation = Arc::clone(self);
        let task = tokio::spawn(async move {
            if started.await.is_err() {
                return Ok(());
            }
            let result = future.await.map_err(|error| error.to_string());
            let failure = returned_failure.and_then(|failure| failure.lock().unwrap().take());
            let recorded = operation.record_root_result(result, failure).await;
            if let Err(error) = &recorded {
                operation.record_failure(error);
            }
            #[cfg(all(test, feature = "testing"))]
            {
                let cut = operation.ordinary_return_cut.lock().unwrap().clone();
                if let Some(cut) = cut {
                    cut.pause().await;
                }
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
        returned_failure: Option<crate::host::state::PublicCallbackFailure>,
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
        if let (Some(failure), PublicOperationDisposition::Failed(_)) =
            (returned_failure, &disposition)
            && !current.lifecycle.outcomes.iter().any(|outcome| {
                matches!(
                    outcome,
                    crate::host::state::PublicInstructionOutcome::RebuildRequired(_)
                )
            })
        {
            let pending = match self
                .store
                .record_public_retry_failure(&current.intent, current.attempt, &failure)
                .await
            {
                Ok(pending) => pending,
                Err(error) => {
                    let pending = self.advance(
                        PublicOperationStage::Running,
                        PublicOperationStage::ContainmentPending,
                        Some(PublicOperationDisposition::Ambiguous(format!(
                            "returned callback failure could not retain its retry receipt: {error}"
                        ))),
                    ).await?;
                    self.record.send_replace(pending);
                    self.bump_revision();
                    return Ok(());
                }
            };
            self.record.send_replace(pending);
            self.bump_revision();
            return Ok(());
        }

        if (!current.intent.class.is_terminal()
            && matches!(disposition, PublicOperationDisposition::Failed(_)))
            || (current.intent.class.is_terminal() && !self.descendants_contained().await)
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
        #[cfg(feature = "testing")]
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
        #[cfg(feature = "testing")]
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
        if self.intent().goal().is_some() {
            self.invalidate_access();
            // Cancellation withdraws authority, not ownership of an accepted
            // ordinary future. Abort/Close is the creation owner's barrier.
            self.join_ordinary_root().await?;
            let _lifecycle = self.lifecycle.lock().await;
            let current = self.snapshot();
            if matches!(
                current.stage,
                PublicOperationStage::Completed | PublicOperationStage::ContainmentPending
            ) {
                return Ok(current);
            }

            return self.persist_cancellation(current, None).await;
        }
        let task = {
            let _lifecycle = self.lifecycle.lock().await;
            let current = self.snapshot();
            if matches!(
                current.stage,
                PublicOperationStage::Completed | PublicOperationStage::ContainmentPending
            ) {
                return Ok(current);
            }
            self.invalidate_access();
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

    async fn join_ordinary_root(&self) -> Result<()> {
        let _join = self.ordinary_join.lock().await;
        let mut retained = self.task.lock().await;
        if let Some(task) = retained.as_mut() {
            #[cfg(all(test, feature = "testing"))]
            if let Some(entered) = self.ordinary_join_entered.lock().unwrap().as_ref() {
                entered.notify_one();
            }
            // Cancellation drops only the observer/guards. The operation keeps
            // its handle until the actual join and sticky outcome are recorded.
            let outcome = task.await;
            retained.take();
            match outcome {
                Ok(Ok(())) => {}
                Ok(Err(error)) => {
                    self.record_failure(&error);
                    return Err(error);
                }
                Err(error) => {
                    let error = HostError::CommandRejected(format!(
                        "ordinary callback owner join failed: {error}"
                    ));
                    self.record_failure(&error);
                    return Err(error);
                }
            }
        }
        if let Some(error) = self.failure.borrow().clone() {
            return Err(HostError::CommandRejected(error));
        }
        Ok(())
    }

    #[cfg(all(test, feature = "testing"))]
    pub(crate) fn notify_ordinary_join(&self, entered: Arc<tokio::sync::Notify>) {
        *self.ordinary_join_entered.lock().unwrap() = Some(entered);
    }

    #[cfg(all(test, feature = "testing"))]
    pub(crate) fn set_ordinary_return_cut(&self, cut: Arc<RegistryTestCut>) {
        *self.ordinary_return_cut.lock().unwrap() = Some(cut);
    }

    async fn persist_cancellation(
        &self,
        current: PublicOperationRecord,
        task_failure: Option<String>,
    ) -> Result<PublicOperationRecord> {
        let no_unsettled_callback = task_failure.is_none()
            && self
                .store
                .public_operation_records()
                .await?
                .iter()
                .any(|record| {
                    record.intent == current.intent
                        && matches!(
                            record.lifecycle.in_flight,
                            None | Some(crate::host::state::PublicInstruction::Revoke)
                        )
                        && !record.lifecycle.outcomes.iter().any(|outcome| {
                            matches!(
                                outcome,
                                crate::host::state::PublicInstructionOutcome::RebuildRequired(_)
                                    | crate::host::state::PublicInstructionOutcome::DataLoss(
                                        crate::host::state::DataLossOutcome::Error(_)
                                    )
                            )
                        })
                });
        let next = if current.stage == PublicOperationStage::RetryPending
            || no_unsettled_callback
            || (current.intent.class.is_terminal() && self.descendants_contained().await)
        {
            PublicOperationStage::Completed
        } else {
            PublicOperationStage::ContainmentPending
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
        self.wait_for_result(None).await
    }

    pub(crate) async fn wait_for_attempt(&self, attempt: u64) -> Result<PublicOperationRecord> {
        self.wait_for_result(Some(attempt)).await
    }

    async fn wait_for_result(&self, attempt: Option<u64>) -> Result<PublicOperationRecord> {
        let mut record = self.record.subscribe();
        let mut failure = self.failure.subscribe();
        loop {
            let current = record.borrow().clone();
            if let Some(attempt) = attempt
                && let Some(outcome) = current.attempt_results.get(&attempt)
            {
                let mut result = current.clone();
                result.attempt = attempt;
                result.stage = outcome.stage;
                result.disposition = Some(outcome.disposition.clone());
                result.lifecycle = outcome.lifecycle.clone();
                result.retry_failure = outcome.retry_failure.clone();
                result.containment = outcome.containment;
                result.superseded_by = outcome.superseded_by.clone();
                result.blockers = outcome.blockers.clone();
                result
                    .attempt_results
                    .retain(|number, _| *number <= attempt);
                return Ok(result);
            }
            if matches!(
                current.stage,
                PublicOperationStage::RetryPending
                    | PublicOperationStage::ContainmentPending
                    | PublicOperationStage::Completed
            ) && attempt.is_none_or(|attempt| current.attempt == attempt)
            {
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
        let finished = {
            let mut task = self.task.lock().await;
            if task.as_ref().is_some_and(JoinHandle::is_finished) {
                task.take()
            } else {
                None
            }
        };
        if let Some(task) = finished {
            let _ = task.await;
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
        let retained_failure = self.failure.borrow().clone();
        if record.stage == PublicOperationStage::RetryPending
            && let Some(error) = retained_failure.clone()
        {
            let pending = self
                .advance(
                    PublicOperationStage::RetryPending,
                    PublicOperationStage::ContainmentPending,
                    Some(PublicOperationDisposition::Ambiguous(error)),
                )
                .await?;
            self.record.send_replace(pending.clone());
            self.recovery_complete();
            return Ok(Some(pending));
        }
        if matches!(
            record.stage,
            PublicOperationStage::Completed
                | PublicOperationStage::ContainmentPending
                | PublicOperationStage::RetryPending
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
            && retained_failure.is_none()
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
        if recovered.stage == PublicOperationStage::Ready {
            self.failure.send_replace(None);
            let _invalidation = self.access_invalidation.lock().unwrap();
            self.access_invalidated
                .store(false, std::sync::atomic::Ordering::Release);
        }
        self.recovery_complete();
        Ok(Some(recovered))
    }

    fn set_coordination_active(&self, active: bool) {
        self.coordination_active
            .store(active, std::sync::atomic::Ordering::Release);
    }

    pub(crate) async fn resume_returned_failure(&self) -> Result<()> {
        let _lifecycle = self.lifecycle.lock().await;
        let current = self.snapshot();
        if current.stage != PublicOperationStage::RetryPending {
            return Ok(());
        }
        if self
            .shutting_down
            .load(std::sync::atomic::Ordering::Acquire)
        {
            return Err(HostError::CommandRejected(
                "public-operation registry is shutting down".into(),
            ));
        }
        if let Some(task) = self.task.lock().await.take() {
            task.await.map_err(|error| {
                HostError::CommandRejected(format!("returned callback task join failed: {error}"))
            })??;
        }
        let ready = self
            .store
            .resume_public_operation(&current.intent, current.attempt)
            .await?;
        self.record.send_replace(ready);
        self.failure.send_replace(None);
        let _invalidation = self.access_invalidation.lock().unwrap();
        self.access_invalidated
            .store(false, std::sync::atomic::Ordering::Release);
        self.recovery_complete();
        self.bump_revision();
        Ok(())
    }

    fn record_failure(&self, error: &HostError) {
        self.invalidate_access();
        self.needs_recovery
            .store(true, std::sync::atomic::Ordering::Release);
        self.failure.send_replace(Some(error.to_string()));
        self.bump_revision();
    }

    #[cfg(all(test, feature = "testing"))]
    pub(crate) fn retained_failure(&self) -> Option<String> {
        self.failure.borrow().clone()
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
            self.invalidate_access();
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

fn configuration_pending_for_fenced_intent(
    renewal: &PublicV1SessionRenewalStatus,
    local_identity: &ReplicaIdentity,
    intent: &PublicOperationIntent,
) -> Option<PublicV1SessionRenewalStatus> {
    let PublicOperationProgram::Configuration(configuration) = intent.program.as_ref()? else {
        return None;
    };
    if intent.class != PublicOperationClass::Authority
        || configuration.current != renewal.required.fresh_configuration
        || configuration.previous.is_some()
        || !renewal
            .required
            .replacement
            .iter()
            .chain(renewal.required.surviving_sessions.iter())
            .any(|endpoint| {
                endpoint.identity == *local_identity
                    && endpoint.process_session_id == intent.process_session_id
            })
    {
        return None;
    }
    align_configuration_pending(renewal)
}

fn native_preparation_pending_for_fenced_intent(
    renewal: &PublicV1SessionRenewalStatus,
    local_identity: &ReplicaIdentity,
    intent: &PublicOperationIntent,
) -> Option<PublicV1SessionRenewalStatus> {
    if intent.class != PublicOperationClass::Authority
        || !renewal_endpoint_matches_intent(renewal, local_identity, intent)
    {
        return None;
    }
    let fresh = &renewal.required.fresh_configuration;
    match intent.program.as_ref()? {
        PublicOperationProgram::Epoch { epoch } if *epoch == fresh.epoch => {}
        PublicOperationProgram::Configuration(configuration)
        | PublicOperationProgram::CatchUp { configuration, .. }
            if configuration.current == *fresh
                && configuration.previous.as_ref().is_some_and(|previous| {
                    previous.primary_id == fresh.primary_id
                        && previous.epoch >= renewal.required.previous_epoch
                        && previous.epoch <= fresh.epoch
                        && previous.write_quorum == fresh.write_quorum
                        && previous.members.len() >= fresh.write_quorum as usize
                        && previous
                            .members
                            .iter()
                            .all(|member| fresh.members.contains(member))
                }) => {}
        _ => return None,
    }
    align_configuration_pending(renewal)
}

fn align_configuration_pending(
    renewal: &PublicV1SessionRenewalStatus,
) -> Option<PublicV1SessionRenewalStatus> {
    let mut aligned = renewal.clone();
    if aligned.stage == PublicSessionRenewalStage::EpochBarriersPending {
        while let Some(barrier) = aligned
            .barriers
            .iter()
            .find(|barrier| {
                barrier.stage
                    == crate::protocol::public_operations::PublicSessionEpochBarrierStage::Pending
            })
            .cloned()
        {
            if !aligned
                .required
                .fresh_configuration
                .members
                .iter()
                .any(|member| member.identity == barrier.survivor.identity)
            {
                return None;
            }
            aligned = aligned
                .record_epoch_updated(&barrier.survivor, aligned.required.renewal_epoch)
                .ok()?;
        }
        aligned = aligned.finish_epoch_barriers().ok()?;
    }
    if aligned.stage == PublicSessionRenewalStage::ReplacementRolePending {
        let replacement = aligned.required.replacement.as_ref()?;
        aligned = aligned
            .replay_replacement_role(
                &replacement.process_session_id,
                aligned.required.renewal_epoch,
                aligned.required.predecessor.role,
            )
            .ok()?;
    }
    (aligned.stage == PublicSessionRenewalStage::ConfigurationPending).then_some(aligned)
}

fn is_configuration_pending_for_fenced_intent(
    renewal: &PublicV1SessionRenewalStatus,
    local_identity: &ReplicaIdentity,
    intent: &PublicOperationIntent,
) -> bool {
    renewal.stage == PublicSessionRenewalStage::ConfigurationPending
        && configuration_pending_for_fenced_intent(renewal, local_identity, intent)
            .as_ref()
            .is_some_and(|aligned| aligned == renewal)
}

fn renewal_endpoint_matches_intent(
    renewal: &PublicV1SessionRenewalStatus,
    local_identity: &ReplicaIdentity,
    intent: &PublicOperationIntent,
) -> bool {
    renewal
        .required
        .replacement
        .iter()
        .chain(renewal.required.surviving_sessions.iter())
        .any(|endpoint| {
            endpoint.identity == *local_identity
                && endpoint.process_session_id == intent.process_session_id
        })
}

fn is_retained_replacement_open(
    renewal: &PublicV1SessionRenewalStatus,
    local: &ReplicaIdentity,
    storage: Option<&crate::protocol::types::ReplicaStorageIncarnationContext>,
    intent: &PublicOperationIntent,
) -> bool {
    renewal.stage == PublicSessionRenewalStage::ReplacementRolePending
        && renewal.required.predecessor_contained
        && intent.class == PublicOperationClass::Authority
        && matches!(intent.program.as_ref(),
            Some(PublicOperationProgram::Open { replica, existing: true }) if replica == local)
        && renewal
            .required
            .replacement
            .as_ref()
            .is_some_and(|replacement| {
                replacement.identity == *local
                    && replacement.process_session_id == intent.process_session_id
                    && replacement.process_session_id
                        != renewal.required.predecessor.process_session_id
                    && replacement.storage_incarnation
                        == renewal.required.predecessor.storage_incarnation
                    && storage == Some(&replacement.storage_incarnation)
            })
}

fn is_contained_predecessor_fault(
    renewal: &PublicV1SessionRenewalStatus,
    record: &PublicOperationRecord,
    incoming: &PublicOperationIntent,
) -> bool {
    renewal.required.predecessor_contained
        && record.stage == PublicOperationStage::Completed
        && record.intent.class == PublicOperationClass::TransientFault
        && record.intent.identity == incoming.identity
        && record.intent.process_session_id == renewal.required.predecessor.process_session_id
        && record.accepted_history.as_ref() == Some(&renewal.required.history)
        && record.accepted_storage.as_ref()
            == Some(&renewal.required.predecessor.storage_incarnation)
        && renewal
            .required
            .replacement
            .as_ref()
            .is_some_and(|replacement| {
                replacement.process_session_id == incoming.process_session_id
                    && replacement.process_session_id != record.intent.process_session_id
                    && replacement.storage_incarnation
                        == renewal.required.predecessor.storage_incarnation
            })
}

pub(crate) fn contained_transient_reopen(
    state: &super::state::AgentState,
    process: &ProcessSessionId,
) -> bool {
    if state.reported_fault != Some(crate::protocol::types::FaultType::Transient) {
        return false;
    }
    let Some(preview) = state.public_v1_operation_state.as_ref() else {
        return false;
    };
    let Some(renewal) = preview.session_renewal.as_ref() else {
        return false;
    };
    let Some(open) = preview
        .current_operation
        .as_ref()
        .and_then(|id| preview.operations.get(id))
    else {
        return false;
    };
    open.intent.process_session_id == *process
        && matches!(
            open.stage,
            PublicOperationStage::Running | PublicOperationStage::CallbackApplied
        )
        && open.superseded_by.is_none()
        && open.accepted_history.as_ref() == Some(&renewal.required.history)
        && open.accepted_storage.as_ref() == preview.storage_incarnation.as_ref()
        && is_retained_replacement_open(
            renewal,
            &state.identity.local_identity,
            preview.storage_incarnation.as_ref(),
            &open.intent,
        )
        && preview.operations.values().any(|fault| {
            is_contained_predecessor_fault(renewal, fault, &open.intent)
                && fault.superseded_by.as_ref() == Some(&open.intent.operation_id)
        })
}

pub(crate) struct PartitionOperationRegistry {
    store: Arc<dyn AgentStore>,
    identity: PublicV1Identity,
    process_session_id: ProcessSessionId,
    admission: Mutex<()>,
    dispatch: std::sync::Mutex<BTreeMap<OperationId, Arc<Mutex<()>>>>,
    state: Mutex<RegistryState>,
    coordination: Mutex<Vec<JoinHandle<()>>>,
    revision: watch::Sender<u64>,
    shutting_down: Arc<std::sync::atomic::AtomicBool>,
    terminal_fence: std::sync::atomic::AtomicBool,
    renewal_fence: std::sync::atomic::AtomicBool,
    control_tasks: std::sync::Mutex<Vec<JoinHandle<()>>>,
    terminal_tasks: std::sync::Mutex<Vec<JoinHandle<()>>>,
    abort_guard: Arc<std::sync::Mutex<bool>>,
    terminal_recovery: std::sync::OnceLock<
        Arc<
            dyn Fn() -> Result<crate::host::public_lifecycle::PublicLifecycleCallbacks>
                + Send
                + Sync,
        >,
    >,
    #[cfg(all(test, feature = "testing"))]
    admission_cut: std::sync::Mutex<Option<(OperationId, Arc<RegistryTestCut>)>>,
    #[cfg(all(test, feature = "testing"))]
    shutdown_cut: std::sync::Mutex<Option<Arc<RegistryTestCut>>>,
    #[cfg(all(test, feature = "testing"))]
    completion_cut: std::sync::Mutex<Option<(OperationId, Arc<RegistryTestCut>)>>,
    #[cfg(all(test, feature = "testing"))]
    drain_cut: std::sync::Mutex<Option<Arc<RegistryTestCut>>>,
}

#[cfg(all(test, feature = "testing"))]
#[derive(Default)]
pub(crate) struct RegistryTestCut {
    pub(crate) entered: tokio::sync::Notify,
    pub(crate) release: tokio::sync::Notify,
}

#[cfg(all(test, feature = "testing"))]
impl RegistryTestCut {
    async fn pause(&self) {
        self.entered.notify_one();
        self.release.notified().await;
    }
}

impl PartitionOperationRegistry {
    pub(crate) fn new(
        store: Arc<dyn AgentStore>,
        preview: PublicV1Identity,
        process_session_id: ProcessSessionId,
    ) -> Result<Arc<Self>> {
        if !preview.is_public_v1() {
            return Err(HostError::CommandRejected(
                "invalid Public V1 identity".into(),
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
            identity: preview,
            process_session_id,
            admission: Mutex::new(()),
            dispatch: Default::default(),
            state: Mutex::new(RegistryState::default()),
            coordination: Mutex::new(Vec::new()),
            revision,
            shutting_down: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            terminal_fence: std::sync::atomic::AtomicBool::new(false),
            renewal_fence: std::sync::atomic::AtomicBool::new(false),
            control_tasks: std::sync::Mutex::new(Vec::new()),
            terminal_tasks: std::sync::Mutex::new(Vec::new()),
            abort_guard: Arc::new(std::sync::Mutex::new(false)),
            terminal_recovery: std::sync::OnceLock::new(),
            #[cfg(all(test, feature = "testing"))]
            admission_cut: Default::default(),
            #[cfg(all(test, feature = "testing"))]
            shutdown_cut: Default::default(),
            #[cfg(all(test, feature = "testing"))]
            completion_cut: Default::default(),
            #[cfg(all(test, feature = "testing"))]
            drain_cut: Default::default(),
        }))
    }

    pub(crate) fn bind_terminal_recovery(
        &self,
        callbacks: impl Fn() -> Result<crate::host::public_lifecycle::PublicLifecycleCallbacks>
        + Send
        + Sync
        + 'static,
    ) -> Result<()> {
        self.terminal_recovery
            .set(Arc::new(callbacks))
            .map_err(|_| HostError::CommandRejected("terminal recovery is already bound".into()))
    }

    pub(crate) fn revision_receiver(&self) -> watch::Receiver<u64> {
        self.revision.subscribe()
    }
    #[cfg(test)]
    pub(crate) async fn operation_for_validation(
        &self,
        id: &OperationId,
    ) -> Option<Arc<PartitionOperation>> {
        self.state.lock().await.operations.get(id).cloned()
    }

    pub(crate) fn fence(&self) {
        self.terminal_fence
            .store(true, std::sync::atomic::Ordering::Release);
    }

    pub(crate) fn fence_session_renewal(&self) {
        self.renewal_fence
            .store(true, std::sync::atomic::Ordering::Release);
    }

    fn terminal_admission_is_fenced(&self) -> bool {
        self.terminal_fence
            .load(std::sync::atomic::Ordering::Acquire)
            || self
                .shutting_down
                .load(std::sync::atomic::Ordering::Acquire)
    }

    pub(crate) fn fence_and_schedule_fault_containment(self: &Arc<Self>) {
        self.fence();
        let registry = Arc::clone(self);
        self.own_control_task(async move {
            let operations = registry
                .state
                .lock()
                .await
                .operations
                .values()
                .cloned()
                .collect::<Vec<_>>();
            for operation in operations {
                if !operation.intent().class.is_terminal()
                    && let Err(error) = operation.cancel_root().await
                {
                    operation.record_failure(&error);
                }
            }
        });
    }

    pub(crate) fn abort_guard(&self) -> Arc<std::sync::Mutex<bool>> {
        self.abort_guard.clone()
    }

    pub(crate) fn is_fenced(&self) -> bool {
        self.renewal_fence
            .load(std::sync::atomic::Ordering::Acquire)
            || self.terminal_admission_is_fenced()
    }

    async fn prepare_fenced_session_renewal_configuration(
        &self,
        intent: &PublicOperationIntent,
    ) -> Result<bool> {
        let state = self.store.load_state().await?;
        let Some((current, aligned)) =
            state
                .public_v1_operation_state
                .as_ref()
                .and_then(|preview| {
                    preview
                        .session_renewal
                        .as_ref()
                        .filter(|renewal| {
                            preview.history.as_ref() == Some(&renewal.required.history)
                        })
                        .and_then(|renewal| {
                            if is_retained_replacement_open(
                                renewal,
                                &state.identity.local_identity,
                                preview.storage_incarnation.as_ref(),
                                intent,
                            ) || crate::host::public_lifecycle::is_replacement_role_replay(
                                renewal,
                                &state.identity.local_identity,
                                intent,
                            ) {
                                return Some((renewal.clone(), renewal.clone()));
                            }
                            configuration_pending_for_fenced_intent(
                                renewal,
                                &state.identity.local_identity,
                                intent,
                            )
                            .or_else(|| {
                                native_preparation_pending_for_fenced_intent(
                                    renewal,
                                    &state.identity.local_identity,
                                    intent,
                                )
                            })
                            .map(|aligned| (renewal.clone(), aligned))
                        })
                })
        else {
            return Ok(false);
        };
        if current != aligned {
            crate::host::service::align_session_renewal(self.store.as_ref(), current, &aligned)
                .await?;
        }
        Ok(true)
    }

    pub(crate) async fn release_session_renewal_fence(
        &self,
        intent: &PublicOperationIntent,
    ) -> Result<()> {
        let state = self.store.load_state().await?;
        let Some(current) = state
            .public_v1_operation_state
            .as_ref()
            .and_then(|preview| preview.session_renewal.as_ref())
            .filter(|renewal| {
                is_configuration_pending_for_fenced_intent(
                    renewal,
                    &state.identity.local_identity,
                    intent,
                )
            })
            .cloned()
        else {
            return Ok(());
        };
        let PublicOperationProgram::Configuration(configuration) =
            intent.program.as_ref().expect("configuration checked")
        else {
            unreachable!()
        };
        let authorized = current
            .install_fresh_configuration(&configuration.current)
            .map_err(|message| HostError::CommandRejected(message.into()))?;
        if !renewal_endpoint_matches_intent(&authorized, &state.identity.local_identity, intent) {
            return Err(HostError::IdentityMismatch(
                "authorized renewal endpoint changed before fence release".into(),
            ));
        }
        self.store
            .persist_public_v1_session_renewal(Some(&current), &authorized)
            .await?;
        self.renewal_fence
            .store(false, std::sync::atomic::Ordering::Release);
        Ok(())
    }

    pub(crate) fn own_control_task(&self, future: impl Future<Output = ()> + Send + 'static) {
        self.control_tasks
            .lock()
            .expect("control task lock")
            .push(tokio::spawn(future));
    }

    pub(crate) fn try_own_control_task(
        &self,
        future: impl Future<Output = ()> + Send + 'static,
    ) -> Result<()> {
        let mut controls = self.control_tasks.lock().expect("control task lock");
        if self
            .shutting_down
            .load(std::sync::atomic::Ordering::Acquire)
        {
            return Err(HostError::CommandRejected(
                "public-operation registry is shutting down".into(),
            ));
        }
        controls.push(tokio::spawn(future));
        Ok(())
    }

    #[cfg(all(test, feature = "testing"))]
    pub(crate) fn set_admission_cut(&self, id: OperationId, cut: Arc<RegistryTestCut>) {
        *self.admission_cut.lock().unwrap() = Some((id, cut));
    }

    #[cfg(all(test, feature = "testing"))]
    pub(crate) fn set_shutdown_cut(&self, cut: Arc<RegistryTestCut>) {
        *self.shutdown_cut.lock().unwrap() = Some(cut);
    }

    #[cfg(all(test, feature = "testing"))]
    pub(crate) fn set_completion_cut(&self, id: OperationId, cut: Arc<RegistryTestCut>) {
        *self.completion_cut.lock().unwrap() = Some((id, cut));
    }

    #[cfg(all(test, feature = "testing"))]
    pub(crate) async fn pause_completion_registration(&self, id: &OperationId) {
        let cut = self
            .completion_cut
            .lock()
            .unwrap()
            .as_ref()
            .filter(|(expected, _)| expected == id)
            .map(|(_, cut)| cut.clone());
        if let Some(cut) = cut {
            cut.pause().await;
        }
    }

    #[cfg(all(test, feature = "testing"))]
    pub(crate) fn set_drain_cut(&self, cut: Arc<RegistryTestCut>) {
        *self.drain_cut.lock().unwrap() = Some(cut);
    }

    #[cfg(all(test, feature = "testing"))]
    pub(crate) async fn owners_drained(&self) -> bool {
        let controls = self.control_tasks.lock().unwrap().is_empty();
        let terminals = self.terminal_tasks.lock().unwrap().is_empty();
        controls && terminals && self.coordination.lock().await.is_empty()
    }

    pub(crate) fn dispatch_lock(&self, id: &OperationId) -> Arc<Mutex<()>> {
        self.dispatch
            .lock()
            .unwrap()
            .entry(id.clone())
            .or_default()
            .clone()
    }

    pub(crate) fn own_terminal_task(&self, future: impl Future<Output = ()> + Send + 'static) {
        self.terminal_tasks
            .lock()
            .expect("terminal task lock")
            .push(tokio::spawn(future));
    }

    pub(crate) async fn join_ordinary_roots(&self) -> Result<()> {
        let operations = self
            .state
            .lock()
            .await
            .operations
            .values()
            .filter(|operation| {
                operation.intent().goal().is_some() && !operation.intent().class.is_terminal()
            })
            .cloned()
            .collect::<Vec<_>>();
        for operation in operations {
            operation.join_ordinary_root().await?;
        }
        Ok(())
    }

    pub(crate) async fn join_ordinary_goal(&self, id: &OperationId) -> Result<()> {
        let operation = self
            .state
            .lock()
            .await
            .operations
            .get(id)
            .cloned()
            .ok_or_else(|| HostError::CommandRejected("ordinary goal owner unavailable".into()))?;
        operation.join_ordinary_root().await
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
        if intent.identity != self.identity || intent.process_session_id != self.process_session_id
        {
            return Err(HostError::IdentityMismatch(
                "public-operation preview or process session changed".into(),
            ));
        }
        let renewal_configuration =
            if !intent.class.is_terminal() && !self.terminal_admission_is_fenced() {
                self.prepare_fenced_session_renewal_configuration(&intent)
                    .await?
            } else {
                false
            };
        if !intent.class.is_terminal()
            && (self.terminal_admission_is_fenced() || (self.is_fenced() && !renewal_configuration))
        {
            return Err(HostError::CommandRejected(
                "preview is synchronously fenced".into(),
            ));
        }

        self.reap_coordination().await;
        #[cfg(all(test, feature = "testing"))]
        {
            let cut = self
                .admission_cut
                .lock()
                .unwrap()
                .as_ref()
                .filter(|(id, _)| *id == intent.operation_id)
                .map(|(_, cut)| cut.clone());
            if let Some(cut) = cut {
                cut.pause().await;
            }
        }
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
        if let Some(goal) = intent.goal() {
            crate::host::ordinary_goal::validate_admission(&self.store.load_state().await?, goal)?;
        }
        if self.terminal_admission_is_fenced() && !intent.class.is_terminal() {
            return Err(HostError::CommandRejected(
                "preview is synchronously fenced".into(),
            ));
        }
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
        let has_history_barrier = !intent.class.is_terminal()
            && self
                .store
                .load_state()
                .await?
                .public_v1_operation_state
                .is_some_and(|preview| !preview.history_barriers.is_empty());
        if has_history_barrier
            && intent.identity.is_public_v1()
            && intent.lifecycle.as_ref().is_some_and(|input| {
                input.possible_data_loss
                    == crate::protocol::public_operations::PossibleDataLossIntent::Possible
                    && input.data_loss_recovery.is_some()
            })
        {
            for operation in &existing {
                operation
                    .recover_if_needed(&self.process_session_id)
                    .await?;
            }
            let renewed = self
                .store
                .renew_public_v1_data_loss_attempt(&intent)
                .await?;
            if let Some(predecessor) = existing.iter().find(|operation| {
                operation.intent().operation_id == renewed.predecessor.intent.operation_id
            }) {
                predecessor.record.send_replace(renewed.predecessor);
                predecessor.recovery_complete();
            }
            if let Some(successor) = renewed.successor {
                let operation = Arc::new(PartitionOperation::new(
                    self.store.clone(),
                    successor,
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
                drop(admission);
                return Ok(operation);
            }
            return Err(HostError::CommandRejected(format!(
                "data-loss renewal is terminal: {:?}",
                renewed.decision
            )));
        }
        if has_history_barrier {
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
        let renewal = if renewal_configuration {
            self.store
                .load_state()
                .await?
                .public_v1_operation_state
                .and_then(|preview| preview.session_renewal)
        } else {
            None
        };
        for operation in existing {
            let record = operation.snapshot();
            if intent.goal().is_some_and(|goal| {
                matches!(
                    goal.desired,
                    crate::protocol::public_operations::ReplicaGoalKind::Discover
                        | crate::protocol::public_operations::ReplicaGoalKind::Prepare { .. }
                ) && record.intent.goal().is_some()
                    && record.intent.process_session_id != intent.process_session_id
            }) {
                continue;
            }
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
            let relation = if record.intent.goal().is_some() && intent.goal().is_some() {
                ordinary_relation(
                    &record.intent,
                    &intent,
                    record.stage == PublicOperationStage::Completed,
                )
            } else if renewal
                .as_ref()
                .is_some_and(|renewal| is_contained_predecessor_fault(renewal, &record, &intent))
            {
                AdmissionRelation::Supersede
            } else if record.stage == PublicOperationStage::Completed {
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
            if intent.class.is_terminal() {
                self.fence();
            }
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
        // Fence only validated registry input/relations, but before durable
        // admission can yield or fail ambiguously. This process never reopens.
        if intent.class.is_terminal() {
            self.fence();
        }
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
                        .spawn_root(async { Ok::<(), HostError>(()) })
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
            } else if displaced
                .wait_for_descendants
                .load(std::sync::atomic::Ordering::Acquire)
            {
                if let Err(error) = displaced.wait_for_witness().await {
                    first_error.get_or_insert(error);
                }
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
            if (matches!(
                operation.intent().program,
                Some(PublicOperationProgram::Close | PublicOperationProgram::Abort)
            ) || (operation.intent().goal().is_some()
                && operation.intent().class.is_terminal()
                && operation.intent().process_session_id == self.process_session_id))
                && operation.snapshot().stage == PublicOperationStage::ContainmentPending
                && let Some(callbacks) = self.terminal_recovery.get()
            {
                crate::host::public_lifecycle::recover_terminal(self, &operation, callbacks()?)
                    .await?;
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
        let admission = self.admission.lock().await;
        let mut first_error = None;
        let operations = self
            .state
            .lock()
            .await
            .operations
            .values()
            .cloned()
            .collect::<Vec<_>>();
        let ordinary = operations
            .iter()
            .any(|operation| operation.intent().goal().is_some());
        // The flag and admit's post-lock check close the gate. Retained
        // dispatchers may still need this mutex to observe that rejection.
        drop(admission);
        #[cfg(all(test, feature = "testing"))]
        {
            let cut = self.shutdown_cut.lock().unwrap().clone();
            if let Some(cut) = cut {
                cut.pause().await;
            }
        }
        let drain_result = if ordinary && let Some(callbacks) = self.terminal_recovery.get() {
            let mut callbacks = callbacks()?;
            callbacks.aborted = self.abort_guard();
            (callbacks.revoke_access)()?;
            callbacks.abort_once();
            let drain = callbacks.terminal_drain.clone();
            let (completed, result) = oneshot::channel();
            self.own_terminal_task(async move {
                let outcome = drain().await;
                if let Err(error) = &outcome {
                    tracing::warn!(%error, "ordinary shutdown remains uncontained");
                }
                let _ = completed.send(outcome);
            });
            Some(result)
        } else {
            None
        };
        for operation in operations {
            if !matches!(
                operation.snapshot().stage,
                PublicOperationStage::Completed | PublicOperationStage::ContainmentPending
            ) && let Err(error) = operation.cancel_root().await
            {
                first_error.get_or_insert(error);
            }
        }
        if let Err(error) = self.drain_owned_tasks(ordinary).await {
            first_error.get_or_insert(error);
        }
        if let Err(error) = self.recover_unowned().await {
            first_error.get_or_insert(error);
        }
        if let Err(error) = self.drain_owned_tasks(ordinary).await {
            first_error.get_or_insert(error);
        }
        if let Some(result) = drain_result {
            match result.await {
                Ok(Ok(())) => {}
                Ok(Err(error)) => {
                    first_error.get_or_insert(HostError::from(error));
                }
                Err(error) => {
                    first_error.get_or_insert_with(|| {
                        HostError::CommandRejected(format!(
                            "ordinary shutdown drain owner lost: {error}"
                        ))
                    });
                }
            }
        }
        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    async fn drain_owned_tasks(&self, ordinary: bool) -> Result<()> {
        let mut first_error = None;
        // Joined owners can register completion/terminal/coordination children.
        // External dispatch registration is closed; drain all three to quiescence.
        loop {
            let controls =
                std::mem::take(&mut *self.control_tasks.lock().expect("control task lock"));
            let terminals =
                std::mem::take(&mut *self.terminal_tasks.lock().expect("terminal task lock"));
            let coordination = std::mem::take(&mut *self.coordination.lock().await);
            #[cfg(all(test, feature = "testing"))]
            {
                let cut = self.drain_cut.lock().unwrap().take();
                if let Some(cut) = cut {
                    cut.pause().await;
                }
            }
            if controls.is_empty() && terminals.is_empty() && coordination.is_empty() {
                break;
            }
            for task in controls {
                if !ordinary {
                    task.abort();
                }
                if let Err(error) = task.await
                    && !error.is_cancelled()
                {
                    first_error.get_or_insert_with(|| {
                        HostError::CommandRejected(format!(
                            "retained control owner join failed: {error}"
                        ))
                    });
                }
            }
            for task in terminals {
                if let Err(error) = task.await {
                    first_error.get_or_insert_with(|| {
                        HostError::CommandRejected(format!("terminal cleanup join failed: {error}"))
                    });
                }
            }
            for task in coordination {
                if let Err(error) = task.await {
                    first_error.get_or_insert_with(|| {
                        HostError::CommandRejected(format!(
                            "public-operation coordination join failed: {error}"
                        ))
                    });
                }
            }
        }
        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    async fn load_durable_records(&self) -> Result<()> {
        let records = self
            .store
            .public_operation_records()
            .await
            .inspect_err(|_| self.fence())?;
        let durable = self
            .store
            .load_state()
            .await
            .inspect_err(|_| self.fence())?;
        if records.iter().any(|record| {
            record.intent.process_session_id == self.process_session_id
                && record.intent.class.is_terminal()
        }) {
            self.fence();
        }
        if durable.public_v1_operation_state.is_some_and(|preview| {
            preview.terminal
                && crate::host::ordinary_goal::terminal_for_process(
                    &preview,
                    &self.process_session_id,
                )
        }) {
            // A contained predecessor fault can remain in a replacement's
            // journal. Only this process's terminal records close it forever.
            self.fence_session_renewal();
        }
        let mut state = self.state.lock().await;
        let durable_ids = records
            .iter()
            .map(|record| record.intent.operation_id.clone())
            .collect::<BTreeSet<_>>();
        state.operations.retain(|id, operation| {
            operation.intent().goal().is_none()
                || durable_ids.contains(id)
                || operation.snapshot().stage != PublicOperationStage::Completed
        });
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

    pub(crate) async fn has_uncontained_predecessors(
        &self,
        operation: &PartitionOperation,
    ) -> bool {
        let blockers = operation.snapshot().blockers;
        let state = self.state.lock().await;
        blockers.iter().any(|id| {
            state.operations.get(id).is_some_and(|predecessor| {
                predecessor.snapshot().stage != PublicOperationStage::Completed
            })
        })
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

fn ordinary_relation(
    existing: &PublicOperationIntent,
    incoming: &PublicOperationIntent,
    completed: bool,
) -> AdmissionRelation {
    use AdmissionRelation::*;
    if existing.process_session_id != incoming.process_session_id {
        return if completed {
            Coexist
        } else {
            Reject("predecessor ordinary creation remains unresolved")
        };
    }
    if existing.class.is_terminal() {
        return Reject("ordinary creation already has an immutable terminal owner");
    }
    if incoming.class.is_terminal() {
        return if completed { Coexist } else { Supersede };
    }
    if incoming.revision <= existing.revision {
        return Reject("ordinary goal revision is not newer");
    }
    if completed {
        return Coexist;
    }
    match (&existing.class, &incoming.class) {
        (
            PublicOperationClass::Build { target: left },
            PublicOperationClass::Build { target: right },
        )
        | (
            PublicOperationClass::Remove { target: left },
            PublicOperationClass::Remove { target: right },
        )
        | (
            PublicOperationClass::Build { target: left },
            PublicOperationClass::Remove { target: right },
        )
        | (
            PublicOperationClass::Remove { target: left },
            PublicOperationClass::Build { target: right },
        ) if left != right => Coexist,
        _ => Reject("unresolved ordinary action requires completion or whole-creation withdrawal"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::sqlite_store::SqliteStore;
    use crate::host::state::{AgentState, StorageIdentity};
    use crate::protocol::public_operations::{
        ProcessSessionRenewalRequired, PublicConfiguration, PublicV1LifecycleBinding,
        PublicV1SessionRenewalStatus, StatePersistence,
    };
    use crate::protocol::types::{
        AgentGeneration, ConfigurationDescriptor, ConfigurationMember, EffectivePolicy, Epoch,
        InitializationId, PodUid, ProcessSessionEndpoint, ProcessSessionRenewalReason,
        PublicProtocolGeneration, PvcUid, ReplicaId, ReplicaInstanceId, ReplicaRole,
        ReplicaStorageIdentity, ReplicaStorageIncarnationContext, ReplicatorCreationId,
        ResourceUid, StorageGeneration, derive_bootstrap_history, derive_history_initialization_id,
    };

    fn intent(class: PublicOperationClass, revision: u64) -> PublicOperationIntent {
        PublicOperationIntent {
            identity: PublicV1Identity::new(1),
            operation_id: OperationId::new(format!("operation-{revision}-{class:?}")),
            revision,
            process_session_id: ProcessSessionId::new("session"),
            class,
            input_digest:
                crate::protocol::public_operations::CanonicalOperationInputDigest::opaque_v1(
                    "kuberic-operation-test-input-v1",
                    format!("digest-{revision}").as_bytes(),
                ),
            replay_disposition:
                crate::protocol::public_operations::PublicOperationReplayDisposition::Convergent,
            lifecycle: None,
            program: None,
        }
    }

    async fn fence_registry(
        root: &std::path::Path,
        session: ProcessSessionId,
    ) -> (Arc<SqliteStore>, Arc<PartitionOperationRegistry>) {
        let store = Arc::new(
            SqliteStore::create_replacement_authorized(
                root.join("fences.sqlite3"),
                AgentState::new(StorageIdentity {
                    schema_version: crate::host::state::SCHEMA_VERSION,
                    resource_uid: ResourceUid::new("fence-resource"),
                    pod_uid: PodUid::new("fence-pod"),
                    pvc_uid: PvcUid::new("fence-pvc"),
                    initialization_id: InitializationId::new("fence-init"),
                    local_identity: ReplicaIdentity {
                        replica_id: ReplicaId::new(1),
                        instance_id: ReplicaInstanceId::new("fence-pod"),
                        agent_generation: AgentGeneration::new("fence-generation"),
                    },
                    effective_policy: EffectivePolicy::fixed(1, 30).unwrap(),
                }),
                PublicV1LifecycleBinding {
                    identity: PublicV1Identity::new(1),
                    resource_uid: ResourceUid::new("fence-resource"),
                    spec_generation: 1,
                    state_persistence: StatePersistence::Persisted,
                },
            )
            .unwrap(),
        );
        let state = store.load_state().await.unwrap();
        let initialization = derive_history_initialization_id(
            &state.identity.resource_uid,
            PublicProtocolGeneration::new(1),
        );
        let history = derive_bootstrap_history(
            &state.identity.resource_uid,
            PublicProtocolGeneration::new(1),
            &initialization,
        );
        let storage = ReplicaStorageIncarnationContext {
            replica: state.identity.local_identity,
            storage: ReplicaStorageIdentity::Persistent {
                pod_uid: state.identity.pod_uid,
                pvc_uid: state.identity.pvc_uid,
            },
            storage_generation: StorageGeneration::new("fence-storage"),
        };
        store
            .initialize_public_v1_history(&initialization, &history, &storage)
            .await
            .unwrap();
        let registry =
            PartitionOperationRegistry::new(store.clone(), PublicV1Identity::new(1), session)
                .unwrap();
        (store, registry)
    }

    fn program_intent(program: PublicOperationProgram, revision: u64) -> PublicOperationIntent {
        let class = match &program {
            PublicOperationProgram::Close => PublicOperationClass::Close,
            PublicOperationProgram::Abort => PublicOperationClass::Abort,
            _ => PublicOperationClass::Authority,
        };
        PublicOperationIntent {
            input_digest: program.digest(),
            replay_disposition: program.replay_disposition(),
            program: Some(program),
            ..intent(class, revision)
        }
    }

    #[tokio::test]
    async fn terminal_admission_fence_precedes_callbacks_but_not_invalid_input() {
        let root = crate::host::tests::tempdir().unwrap();
        let (store, registry) = fence_registry(root.path(), ProcessSessionId::new("session")).await;
        let epoch = program_intent(
            PublicOperationProgram::Epoch {
                epoch: Epoch::new(0, 1),
            },
            1,
        );
        registry.admit(epoch.clone()).await.unwrap();
        let terminal = program_intent(PublicOperationProgram::Close, 2);
        for invalid in [
            PublicOperationIntent {
                operation_id: epoch.operation_id,
                ..terminal.clone()
            },
            PublicOperationIntent {
                process_session_id: ProcessSessionId::new("stale-session"),
                ..terminal.clone()
            },
            PublicOperationIntent {
                identity: PublicV1Identity::new(2),
                ..terminal.clone()
            },
            PublicOperationIntent {
                input_digest: PublicOperationProgram::Abort.digest(),
                ..terminal.clone()
            },
        ] {
            assert!(registry.admit(invalid).await.is_err());
            assert!(!registry.is_fenced());
        }
        registry.admit(terminal.clone()).await.unwrap();
        assert!(registry.terminal_admission_is_fenced());
        assert!(registry.is_fenced());
        assert!(
            store
                .load_state()
                .await
                .unwrap()
                .public_v1_operation_state
                .unwrap()
                .terminal
        );
        registry.fence_session_renewal();
        registry
            .renewal_fence
            .store(false, std::sync::atomic::Ordering::Release);
        assert!(registry.is_fenced());
        assert!(
            registry
                .admit(program_intent(
                    PublicOperationProgram::Epoch {
                        epoch: Epoch::new(0, 2)
                    },
                    3
                ))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn terminal_fence_recovery_is_current_process_not_predecessor_terminal() {
        let root = crate::host::tests::tempdir().unwrap();
        let (store, registry) = fence_registry(root.path(), ProcessSessionId::new("session")).await;
        let fault = intent(PublicOperationClass::TransientFault, 1);
        store
            .begin_public_operation(&fault, &[], &[])
            .await
            .unwrap();
        registry.load_durable_records().await.unwrap();
        assert!(registry.terminal_admission_is_fenced());
        let replacement = PartitionOperationRegistry::new(
            store,
            PublicV1Identity::new(1),
            ProcessSessionId::new("replacement"),
        )
        .unwrap();
        replacement.load_durable_records().await.unwrap();
        assert!(!replacement.terminal_admission_is_fenced());
        assert!(replacement.is_fenced());
    }

    #[tokio::test]
    async fn uncertain_terminal_admission_and_store_read_failure_remain_fenced() {
        let root = crate::host::tests::tempdir().unwrap();
        let (store, registry) = fence_registry(root.path(), ProcessSessionId::new("session")).await;
        let connection = rusqlite::Connection::open(root.path().join("fences.sqlite3")).unwrap();
        connection
            .execute_batch(
                "CREATE TRIGGER fail_admission BEFORE UPDATE ON agent_state
             BEGIN SELECT RAISE(FAIL, 'injected admission persistence failure'); END;",
            )
            .unwrap();
        let error = registry
            .admit(program_intent(PublicOperationProgram::Abort, 1))
            .await
            .err()
            .expect("injected failure");
        assert!(
            error
                .to_string()
                .contains("injected admission persistence failure"),
            "{error}"
        );
        assert!(registry.terminal_admission_is_fenced());
        assert!(store.public_operation_records().await.unwrap().is_empty());
        connection
            .execute_batch("DROP TRIGGER fail_admission; DROP TABLE agent_state;")
            .unwrap();
        let replacement = PartitionOperationRegistry::new(
            store,
            PublicV1Identity::new(1),
            ProcessSessionId::new("replacement"),
        )
        .unwrap();
        assert!(replacement.load_durable_records().await.is_err());
        assert!(replacement.terminal_admission_is_fenced());
    }

    #[tokio::test]
    async fn retained_renewal_cannot_clear_current_terminal_admission() {
        let resource = ResourceUid::new("resource-renewal-inbound");
        let generation = PublicProtocolGeneration::new(1);
        let initialization = derive_history_initialization_id(&resource, generation);
        let history = derive_bootstrap_history(&resource, generation, &initialization);
        let endpoint = |id, role| {
            let identity = ReplicaIdentity {
                replica_id: ReplicaId::new(id),
                instance_id: ReplicaInstanceId::new(format!("pod-{id}")),
                agent_generation: AgentGeneration::new(format!("generation-{id}")),
            };
            ProcessSessionEndpoint {
                storage_incarnation: ReplicaStorageIncarnationContext {
                    replica: identity.clone(),
                    storage: ReplicaStorageIdentity::Persistent {
                        pod_uid: PodUid::new(format!("pod-{id}")),
                        pvc_uid: PvcUid::new(format!("pvc-{id}")),
                    },
                    storage_generation: StorageGeneration::new(format!("storage-{id}")),
                },
                identity,
                process_session_id: ProcessSessionId::new(format!("session-{id}")),
                replicator_creation_id: ReplicatorCreationId::new(format!("creation-{id}")),
                role,
            }
        };
        let source = endpoint(1, ReplicaRole::Primary);
        let predecessor = endpoint(2, ReplicaRole::ActiveSecondary);
        let replacement = ProcessSessionEndpoint {
            process_session_id: ProcessSessionId::new("replacement-session"),
            replicator_creation_id: ReplicatorCreationId::new("replacement-creation"),
            role: ReplicaRole::None,
            ..predecessor.clone()
        };
        let epoch = Epoch::new(0, 2);
        let required = PublicV1SessionRenewalStatus::required(ProcessSessionRenewalRequired {
            operation_id: OperationId::new("renewal-inbound"),
            reason: ProcessSessionRenewalReason::ActiveSecondaryReplacement,
            resource_uid: resource.clone(),
            history: history.clone(),
            previous_epoch: Epoch::new(0, 1),
            renewal_epoch: epoch,
            predecessor: predecessor.clone(),
            replacement: Some(replacement.clone()),
            surviving_sessions: vec![source.clone()],
            fresh_configuration: ConfigurationDescriptor::new(
                epoch,
                source.identity.replica_id,
                vec![
                    ConfigurationMember {
                        identity: source.identity.clone(),
                        role: ReplicaRole::Primary,
                    },
                    ConfigurationMember {
                        identity: replacement.identity.clone(),
                        role: ReplicaRole::ActiveSecondary,
                    },
                ],
                2,
            ),
            predecessor_contained: true,
            data_loss: None,
        })
        .unwrap();
        let role_pending = required
            .close_access()
            .unwrap()
            .freeze_surviving_set()
            .unwrap()
            .begin_epoch_barriers()
            .unwrap()
            .record_epoch_updated(&source, epoch)
            .unwrap()
            .finish_epoch_barriers()
            .unwrap();
        let pending = role_pending
            .replay_replacement_role(
                &replacement.process_session_id,
                epoch,
                ReplicaRole::ActiveSecondary,
            )
            .unwrap();
        pending.validate().unwrap();
        let root = crate::host::tests::tempdir().unwrap();
        let (store, registry) =
            fence_registry(root.path(), replacement.process_session_id.clone()).await;
        let before = store.load_state().await.unwrap();
        registry.fence_session_renewal();
        assert!(registry.is_fenced());
        registry.fence();
        assert!(registry.terminal_admission_is_fenced());
        registry
            .release_session_renewal_fence(&program_intent(
                PublicOperationProgram::Configuration(
                    crate::protocol::public_operations::PublicConfiguration {
                        current: pending.required.fresh_configuration.clone(),
                        previous: None,
                        replicas: Vec::new(),
                    },
                ),
                1,
            ))
            .await
            .unwrap();
        assert!(registry.is_fenced());
        assert!(registry.terminal_admission_is_fenced());
        assert_eq!(store.load_state().await.unwrap(), before);
    }

    #[test]
    fn fenced_registry_pairs_configuration_with_exact_renewal_stage() {
        let resource_uid = ResourceUid::new("resource-renewal-fence");
        let generation = PublicProtocolGeneration::new(1);
        let initialization = derive_history_initialization_id(&resource_uid, generation);
        let history = derive_bootstrap_history(&resource_uid, generation, &initialization);
        let identity = ReplicaIdentity {
            replica_id: ReplicaId::new(1),
            instance_id: ReplicaInstanceId::new("pod-1"),
            agent_generation: AgentGeneration::new("generation-1"),
        };
        let storage = ReplicaStorageIncarnationContext {
            replica: identity.clone(),
            storage: ReplicaStorageIdentity::Persistent {
                pod_uid: PodUid::new("pod-1"),
                pvc_uid: PvcUid::new("pvc-1"),
            },
            storage_generation: StorageGeneration::new("storage-1"),
        };
        let predecessor = ProcessSessionEndpoint {
            identity: identity.clone(),
            storage_incarnation: storage.clone(),
            process_session_id: ProcessSessionId::new("old-session"),
            replicator_creation_id: ReplicatorCreationId::new("old-creation"),
            role: ReplicaRole::Primary,
        };
        let replacement = ProcessSessionEndpoint {
            process_session_id: ProcessSessionId::new("new-session"),
            replicator_creation_id: ReplicatorCreationId::new("new-creation"),
            role: ReplicaRole::None,
            ..predecessor.clone()
        };
        let epoch = Epoch::new(0, 2);
        let configuration = ConfigurationDescriptor::new(
            epoch,
            identity.replica_id,
            vec![ConfigurationMember {
                identity: identity.clone(),
                role: ReplicaRole::Primary,
            }],
            1,
        );
        let required = PublicV1SessionRenewalStatus::required(ProcessSessionRenewalRequired {
            operation_id: OperationId::new("renewal-fence"),
            reason: ProcessSessionRenewalReason::PrimaryReplacement,
            resource_uid,
            history,
            previous_epoch: Epoch::new(0, 1),
            renewal_epoch: epoch,
            predecessor,
            replacement: Some(replacement.clone()),
            surviving_sessions: Vec::new(),
            fresh_configuration: configuration.clone(),
            predecessor_contained: true,
            data_loss: None,
        })
        .unwrap();
        let access_closed = required.close_access().unwrap();
        let frozen = access_closed.freeze_surviving_set().unwrap();
        let barriers = frozen.begin_epoch_barriers().unwrap();
        let role_pending = barriers.finish_epoch_barriers().unwrap();
        let configuration_pending = role_pending
            .replay_replacement_role(&replacement.process_session_id, epoch, ReplicaRole::Primary)
            .unwrap();
        let authorized = configuration_pending
            .install_fresh_configuration(&configuration)
            .unwrap();
        let configuration_program = PublicOperationProgram::Configuration(PublicConfiguration {
            current: configuration.clone(),
            previous: None,
            replicas: Vec::new(),
        });
        let intent_for = |program: PublicOperationProgram, revision: u64| PublicOperationIntent {
            identity: PublicV1Identity::new(generation.value()),
            operation_id: OperationId::new(format!("renewal-operation-{revision}")),
            revision,
            process_session_id: replacement.process_session_id.clone(),
            class: PublicOperationClass::Authority,
            input_digest: program.digest(),
            replay_disposition: program.replay_disposition(),
            lifecycle: None,
            program: Some(program),
        };
        let configuration_intent = intent_for(configuration_program.clone(), 1);
        let retained_open = intent_for(
            PublicOperationProgram::Open {
                replica: identity.clone(),
                existing: true,
            },
            7,
        );
        assert!(is_retained_replacement_open(
            &role_pending,
            &identity,
            Some(&storage),
            &retained_open,
        ));
        assert!(!is_retained_replacement_open(
            &configuration_pending,
            &identity,
            Some(&storage),
            &retained_open,
        ));
        for invalid in [
            intent_for(
                PublicOperationProgram::Open {
                    replica: identity.clone(),
                    existing: false,
                },
                8,
            ),
            PublicOperationIntent {
                process_session_id: ProcessSessionId::new("old-session"),
                ..retained_open.clone()
            },
        ] {
            assert!(!is_retained_replacement_open(
                &role_pending,
                &identity,
                Some(&storage),
                &invalid,
            ));
        }
        let mut uncontained = role_pending.clone();
        uncontained.required.predecessor_contained = false;
        assert!(!is_retained_replacement_open(
            &uncontained,
            &identity,
            Some(&storage),
            &retained_open,
        ));
        let epoch_intent = intent_for(PublicOperationProgram::Epoch { epoch }, 2);
        let role_intent = intent_for(
            PublicOperationProgram::Role {
                epoch,
                role: ReplicaRole::Primary,
            },
            3,
        );
        let replay_intent = PublicOperationIntent {
            operation_id: crate::host::public_lifecycle::replacement_role_operation_id(&required),
            ..role_intent.clone()
        };
        let stages = [
            &required,
            &access_closed,
            &frozen,
            &barriers,
            &role_pending,
            &configuration_pending,
            &authorized,
        ];

        for status in stages {
            assert_eq!(
                crate::host::public_lifecycle::is_replacement_role_replay(
                    status,
                    &identity,
                    &replay_intent,
                ),
                status.stage == PublicSessionRenewalStage::ReplacementRolePending
            );
            assert_eq!(
                is_configuration_pending_for_fenced_intent(
                    status,
                    &identity,
                    &configuration_intent
                ),
                status.stage == PublicSessionRenewalStage::ConfigurationPending
            );
            assert!(!is_configuration_pending_for_fenced_intent(
                status,
                &identity,
                &epoch_intent
            ));
            assert!(!is_configuration_pending_for_fenced_intent(
                status,
                &identity,
                &role_intent
            ));
        }
        for invalid in [
            role_intent.clone(),
            PublicOperationIntent {
                process_session_id: ProcessSessionId::new("old-session"),
                ..replay_intent.clone()
            },
            PublicOperationIntent {
                program: Some(PublicOperationProgram::Role {
                    epoch: Epoch::new(0, 1),
                    role: ReplicaRole::Primary,
                }),
                ..replay_intent.clone()
            },
            PublicOperationIntent {
                program: Some(PublicOperationProgram::Role {
                    epoch,
                    role: ReplicaRole::ActiveSecondary,
                }),
                ..replay_intent.clone()
            },
        ] {
            assert!(!crate::host::public_lifecycle::is_replacement_role_replay(
                &role_pending,
                &identity,
                &invalid,
            ));
        }
        assert_eq!(
            configuration_pending_for_fenced_intent(&barriers, &identity, &configuration_intent)
                .unwrap(),
            configuration_pending
        );
        assert!(
            configuration_pending_for_fenced_intent(&required, &identity, &configuration_intent)
                .is_none()
        );
        assert!(!is_configuration_pending_for_fenced_intent(
            &configuration_pending,
            &identity,
            &intent(PublicOperationClass::Authority, 1)
        ));
        assert_eq!(
            native_preparation_pending_for_fenced_intent(
                &configuration_pending,
                &identity,
                &epoch_intent,
            ),
            Some(configuration_pending.clone()),
        );
        assert!(
            native_preparation_pending_for_fenced_intent(&required, &identity, &epoch_intent,)
                .is_none()
        );
        for invalid in [
            intent_for(
                PublicOperationProgram::Epoch {
                    epoch: Epoch::new(0, 3),
                },
                4,
            ),
            PublicOperationIntent {
                process_session_id: ProcessSessionId::new("old-session"),
                ..epoch_intent.clone()
            },
            role_intent,
        ] {
            assert!(
                native_preparation_pending_for_fenced_intent(
                    &configuration_pending,
                    &identity,
                    &invalid,
                )
                .is_none()
            );
        }
        let mut joint = PublicConfiguration {
            current: configuration,
            previous: Some(ConfigurationDescriptor::new(
                Epoch::new(0, 1),
                identity.replica_id,
                vec![ConfigurationMember {
                    identity: identity.clone(),
                    role: ReplicaRole::Primary,
                }],
                1,
            )),
            replicas: Vec::new(),
        };
        let joint_intent = intent_for(
            PublicOperationProgram::CatchUp {
                configuration: joint.clone(),
                mode: crate::protocol::public_operations::PublicCatchUpMode::WriteQuorum,
            },
            5,
        );
        assert!(
            native_preparation_pending_for_fenced_intent(
                &configuration_pending,
                &identity,
                &joint_intent,
            )
            .is_some()
        );
        assert!(!is_configuration_pending_for_fenced_intent(
            &configuration_pending,
            &identity,
            &joint_intent,
        ));
        joint.previous.as_mut().unwrap().epoch = Epoch::new(1, 1);
        assert!(
            native_preparation_pending_for_fenced_intent(
                &configuration_pending,
                &identity,
                &intent_for(PublicOperationProgram::Configuration(joint), 6),
            )
            .is_none()
        );
    }

    #[tokio::test]
    async fn renewal_configuration_releases_fence_only_after_durable_authorization() {
        assert_renewal_configuration_release(false).await;
        assert_renewal_configuration_release(true).await;
    }

    async fn assert_renewal_configuration_release(terminal: bool) {
        let root = tempfile::tempdir().unwrap();
        let resource_uid = ResourceUid::new("resource-renewal-release");
        let generation = PublicProtocolGeneration::new(1);
        let preview = PublicV1Identity::new(generation.value());
        let initialization = derive_history_initialization_id(&resource_uid, generation);
        let history = derive_bootstrap_history(&resource_uid, generation, &initialization);
        let identity = ReplicaIdentity {
            replica_id: ReplicaId::new(1),
            instance_id: ReplicaInstanceId::new("pod-release"),
            agent_generation: AgentGeneration::new("generation-release"),
        };
        let storage = ReplicaStorageIncarnationContext {
            replica: identity.clone(),
            storage: ReplicaStorageIdentity::Persistent {
                pod_uid: PodUid::new("pod-release"),
                pvc_uid: PvcUid::new("pvc-release"),
            },
            storage_generation: StorageGeneration::new("storage-release"),
        };
        let predecessor = ProcessSessionEndpoint {
            identity: identity.clone(),
            storage_incarnation: storage.clone(),
            process_session_id: ProcessSessionId::new("old-session"),
            replicator_creation_id: ReplicatorCreationId::new("old-creation"),
            role: ReplicaRole::Primary,
        };
        let replacement = ProcessSessionEndpoint {
            process_session_id: ProcessSessionId::new("new-session"),
            replicator_creation_id: ReplicatorCreationId::new("new-creation"),
            role: ReplicaRole::None,
            ..predecessor.clone()
        };
        let epoch = Epoch::new(0, 2);
        let configuration = ConfigurationDescriptor::new(
            epoch,
            identity.replica_id,
            vec![ConfigurationMember {
                identity: identity.clone(),
                role: ReplicaRole::Primary,
            }],
            1,
        );
        let required = PublicV1SessionRenewalStatus::required(ProcessSessionRenewalRequired {
            operation_id: OperationId::new("renewal-release"),
            reason: ProcessSessionRenewalReason::PrimaryReplacement,
            resource_uid: resource_uid.clone(),
            history: history.clone(),
            previous_epoch: Epoch::new(0, 1),
            renewal_epoch: epoch,
            predecessor,
            replacement: Some(replacement.clone()),
            surviving_sessions: Vec::new(),
            fresh_configuration: configuration.clone(),
            predecessor_contained: true,
            data_loss: None,
        })
        .unwrap();
        let configuration_pending = required
            .close_access()
            .unwrap()
            .freeze_surviving_set()
            .unwrap()
            .begin_epoch_barriers()
            .unwrap()
            .finish_epoch_barriers()
            .unwrap()
            .replay_replacement_role(&replacement.process_session_id, epoch, ReplicaRole::Primary)
            .unwrap();
        let store = Arc::new(
            SqliteStore::create_replacement_authorized(
                root.path().join("agent.sqlite3"),
                AgentState::new(StorageIdentity {
                    schema_version: crate::host::state::SCHEMA_VERSION,
                    resource_uid: resource_uid.clone(),
                    pod_uid: PodUid::new("pod-release"),
                    pvc_uid: PvcUid::new("pvc-release"),
                    initialization_id: InitializationId::new("init-release"),
                    local_identity: identity.clone(),
                    effective_policy: EffectivePolicy::fixed(1, 30).unwrap(),
                }),
                PublicV1LifecycleBinding {
                    identity: preview.clone(),
                    resource_uid,
                    spec_generation: 1,
                    state_persistence: StatePersistence::Persisted,
                },
            )
            .unwrap(),
        );
        store
            .initialize_public_v1_history(&initialization, &history, &storage)
            .await
            .unwrap();
        store
            .persist_public_v1_session_renewal(None, &required)
            .await
            .unwrap();
        let program = PublicOperationProgram::Configuration(PublicConfiguration {
            current: configuration,
            previous: None,
            replicas: Vec::new(),
        });
        let intent = PublicOperationIntent {
            identity: preview.clone(),
            operation_id: OperationId::new("renewal-release-configuration"),
            revision: 1,
            process_session_id: replacement.process_session_id,
            class: PublicOperationClass::Authority,
            input_digest: program.digest(),
            replay_disposition: program.replay_disposition(),
            lifecycle: None,
            program: Some(program),
        };
        let registry = PartitionOperationRegistry::new(
            store.clone(),
            preview,
            intent.process_session_id.clone(),
        )
        .unwrap();
        registry.fence_session_renewal();

        assert!(
            !registry
                .prepare_fenced_session_renewal_configuration(&intent)
                .await
                .unwrap()
        );
        registry
            .release_session_renewal_fence(&intent)
            .await
            .unwrap();
        assert!(registry.is_fenced());
        assert_eq!(
            store
                .load_state()
                .await
                .unwrap()
                .public_v1_operation_state
                .unwrap()
                .session_renewal
                .unwrap()
                .stage,
            PublicSessionRenewalStage::Required
        );

        crate::host::service::align_session_renewal(
            store.as_ref(),
            required,
            &configuration_pending,
        )
        .await
        .unwrap();
        let epoch_program = PublicOperationProgram::Epoch { epoch };
        let epoch_intent = PublicOperationIntent {
            input_digest: epoch_program.digest(),
            replay_disposition: epoch_program.replay_disposition(),
            program: Some(epoch_program),
            operation_id: OperationId::new("renewal-release-epoch"),
            ..intent.clone()
        };
        assert!(
            registry
                .prepare_fenced_session_renewal_configuration(&epoch_intent)
                .await
                .unwrap()
        );
        registry
            .release_session_renewal_fence(&epoch_intent)
            .await
            .unwrap();
        assert!(registry.is_fenced());
        assert!(
            registry
                .prepare_fenced_session_renewal_configuration(&intent)
                .await
                .unwrap()
        );
        assert!(registry.is_fenced());
        if terminal {
            registry.fence();
        }
        registry
            .release_session_renewal_fence(&intent)
            .await
            .unwrap();
        assert_eq!(registry.is_fenced(), terminal);
        assert_eq!(
            store
                .load_state()
                .await
                .unwrap()
                .public_v1_operation_state
                .unwrap()
                .session_renewal
                .unwrap()
                .stage,
            PublicSessionRenewalStage::Authorized
        );
    }

    #[test]
    fn admission_relation_covers_public_v1_operation_edges() {
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
