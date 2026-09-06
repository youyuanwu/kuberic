//! Execution-keyed checkpoint measurements for the durable switchover pilot.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use kuberic_durable_execution::{
    CasOutcome, CheckpointEnvelope, CheckpointPayload, CheckpointState, CheckpointStore,
    ExecutionId, HostOutcome, PersistenceBoundary, StorageRevision, StoreError, StoreErrorKind,
    StoredCheckpoint,
};
use tracing::info;

use super::pilot::{PilotActivityKind, PilotCheckpointStore, decode_pilot_activity_input};

// COMPLEXITY-BOUNDARY: pilot-store:start
const MAX_RECENT_CHECKPOINT_EVENTS: usize = 64;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PilotCheckpointMeasurementsSnapshot {
    pub load_attempts: u64,
    pub write_attempts: u64,
    pub accepted_writes: u64,
    pub conflicts: u64,
    pub unknown_outcomes: u64,
    pub definite_failures: u64,
    pub latest_authoritative_checkpoint_bytes: Option<usize>,
    pub maximum_authoritative_checkpoint_bytes: usize,
    pub latest_active_checkpoint_bytes: Option<usize>,
    pub maximum_active_checkpoint_bytes: usize,
    pub latest_terminal_checkpoint_bytes: Option<usize>,
    pub maximum_terminal_checkpoint_bytes: usize,
    pub completed_activity_count: Option<u64>,
    pub completed_external_effect_count: Option<u64>,
    pub completed_passive_observation_count: Option<u64>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PilotCheckpointEventResult {
    Accepted,
    Conflict,
    OutcomeUnknown,
    DefiniteFailure,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PilotCheckpointEvent {
    pub sequence: u64,
    pub execution_id: ExecutionId,
    pub result: PilotCheckpointEventResult,
    pub attempted_checkpoint_bytes: usize,
    pub authoritative_checkpoint_bytes: Option<usize>,
    pub boundary: Option<PersistenceBoundary>,
}

#[derive(Clone, Default)]
pub struct PilotCheckpointEventCollector {
    events: Arc<Mutex<Vec<PilotCheckpointEvent>>>,
}

impl PilotCheckpointEventCollector {
    pub fn events(&self) -> Vec<PilotCheckpointEvent> {
        self.events
            .lock()
            .expect("pilot checkpoint event collector lock poisoned")
            .clone()
    }

    fn push(&self, event: PilotCheckpointEvent) {
        let mut events = self
            .events
            .lock()
            .expect("pilot checkpoint event collector lock poisoned");
        if events.len() == MAX_RECENT_CHECKPOINT_EVENTS {
            events.remove(0);
        }
        events.push(event);
    }

    fn correlate_latest(&self, boundary: PersistenceBoundary) {
        if let Some(event) = self
            .events
            .lock()
            .expect("pilot checkpoint event collector lock poisoned")
            .iter_mut()
            .rev()
            .find(|event| event.boundary.is_none())
        {
            event.boundary = Some(boundary);
        }
    }
}

#[derive(Clone)]
pub struct MeasuredPilotCheckpointStore {
    execution_id: ExecutionId,
    inner: PilotCheckpointStore,
    measurements: Arc<Mutex<PilotCheckpointMeasurementsSnapshot>>,
    collector: PilotCheckpointEventCollector,
}

impl MeasuredPilotCheckpointStore {
    pub fn new(execution_id: ExecutionId, inner: PilotCheckpointStore) -> Self {
        Self::with_collector(
            execution_id,
            inner,
            PilotCheckpointEventCollector::default(),
        )
    }

    pub fn with_collector(
        execution_id: ExecutionId,
        inner: PilotCheckpointStore,
        collector: PilotCheckpointEventCollector,
    ) -> Self {
        Self {
            execution_id,
            inner,
            measurements: Arc::new(Mutex::new(PilotCheckpointMeasurementsSnapshot::default())),
            collector,
        }
    }

    pub fn measurements(&self) -> PilotCheckpointMeasurementsSnapshot {
        *self
            .measurements
            .lock()
            .expect("pilot checkpoint measurements lock poisoned")
    }

    pub fn collector(&self) -> PilotCheckpointEventCollector {
        self.collector.clone()
    }

    pub fn correlate_host_outcome(&self, outcome: &HostOutcome) {
        let boundary = match outcome {
            HostOutcome::ScheduleAccepted { .. } => Some(PersistenceBoundary::Schedule),
            HostOutcome::DispatchPermitted { boundary, .. } => Some(*boundary),
            HostOutcome::ObservationAccepted { .. } => Some(PersistenceBoundary::Observation),
            HostOutcome::WorkflowCompleted { boundary, .. } => Some(*boundary),
            HostOutcome::ReloadRequired { boundary, .. } => Some(*boundary),
            HostOutcome::StoreFailed {
                operation: kuberic_durable_execution::StoreOperation::CompareAndSwap(boundary),
                ..
            } => Some(*boundary),
            _ => None,
        };
        if let Some(boundary) = boundary {
            self.collector.correlate_latest(boundary);
            let measurements = self.measurements();
            info!(
                execution_id = %self.execution_id,
                persistence_boundary = ?boundary,
                latest_authoritative_checkpoint_bytes =
                    ?measurements.latest_authoritative_checkpoint_bytes,
                maximum_authoritative_checkpoint_bytes =
                    measurements.maximum_authoritative_checkpoint_bytes,
                maximum_active_checkpoint_bytes = measurements.maximum_active_checkpoint_bytes,
                latest_terminal_checkpoint_bytes =
                    ?measurements.latest_terminal_checkpoint_bytes,
                completed_activity_count = ?measurements.completed_activity_count,
                "durable switchover checkpoint boundary"
            );
        }
    }

    fn require_execution(&self, execution_id: ExecutionId) -> Result<(), StoreError> {
        if execution_id == self.execution_id {
            Ok(())
        } else {
            Err(StoreError::new(
                StoreErrorKind::Other,
                "pilot checkpoint store received another execution identity",
            ))
        }
    }

    fn record_authoritative_checkpoint(&self, checkpoint: &CheckpointEnvelope, bytes: usize) {
        let mut measurements = self
            .measurements
            .lock()
            .expect("pilot checkpoint measurements lock poisoned");
        measurements.latest_authoritative_checkpoint_bytes = Some(bytes);
        measurements.maximum_authoritative_checkpoint_bytes = measurements
            .maximum_authoritative_checkpoint_bytes
            .max(bytes);
        let Ok(payload) =
            serde_json::from_slice::<CheckpointPayload>(checkpoint.payload().as_slice())
        else {
            return;
        };
        match payload.state() {
            CheckpointState::Active { activities, .. } => {
                measurements.latest_active_checkpoint_bytes = Some(bytes);
                measurements.maximum_active_checkpoint_bytes =
                    measurements.maximum_active_checkpoint_bytes.max(bytes);
                let mut external_effects = 0_u64;
                let mut passive_observations = 0_u64;
                for activity in activities {
                    let Ok(input) = decode_pilot_activity_input(activity.input()) else {
                        continue;
                    };
                    match input.kind {
                        PilotActivityKind::PassiveObservation => {
                            passive_observations = passive_observations.saturating_add(1);
                        }
                        PilotActivityKind::PreparedReplica { .. }
                        | PilotActivityKind::PreparedLabel { .. } => {
                            external_effects = external_effects.saturating_add(1);
                        }
                    }
                }
                measurements.completed_external_effect_count = Some(external_effects);
                measurements.completed_passive_observation_count = Some(passive_observations);
            }
            CheckpointState::Terminal {
                completed_activity_count,
                ..
            } => {
                measurements.latest_terminal_checkpoint_bytes = Some(bytes);
                measurements.maximum_terminal_checkpoint_bytes =
                    measurements.maximum_terminal_checkpoint_bytes.max(bytes);
                measurements.completed_activity_count = Some(*completed_activity_count);
                let classified_count = measurements
                    .completed_external_effect_count
                    .zip(measurements.completed_passive_observation_count)
                    .map(|(external, passive)| external.saturating_add(passive));
                if classified_count != Some(*completed_activity_count) {
                    measurements.completed_external_effect_count = None;
                    measurements.completed_passive_observation_count = None;
                }
            }
        }
    }
}

#[async_trait]
impl CheckpointStore for MeasuredPilotCheckpointStore {
    async fn load(
        &self,
        execution_id: ExecutionId,
    ) -> Result<Option<StoredCheckpoint>, StoreError> {
        self.require_execution(execution_id)?;
        {
            let mut measurements = self
                .measurements
                .lock()
                .expect("pilot checkpoint measurements lock poisoned");
            measurements.load_attempts = measurements.load_attempts.saturating_add(1);
        }
        let result = self.inner.load(execution_id).await;
        match &result {
            Ok(Some(stored)) => {
                let bytes = stored.checkpoint().encoded_len().map_err(|error| {
                    StoreError::new(
                        StoreErrorKind::MalformedResponse,
                        format!("measure loaded pilot checkpoint: {error}"),
                    )
                })?;
                self.record_authoritative_checkpoint(stored.checkpoint(), bytes);
                info!(
                    execution_id = %execution_id,
                    operation = "load",
                    result = "authoritative",
                    checkpoint_bytes = bytes,
                    "durable switchover checkpoint"
                );
            }
            Ok(None) => info!(
                execution_id = %execution_id,
                operation = "load",
                result = "absent",
                "durable switchover checkpoint"
            ),
            Err(error) => info!(
                execution_id = %execution_id,
                operation = "load",
                result = "definite_failure",
                error_kind = %error.kind(),
                "durable switchover checkpoint"
            ),
        }
        result
    }

    async fn compare_and_swap(
        &self,
        execution_id: ExecutionId,
        expected: Option<StorageRevision>,
        checkpoint: CheckpointEnvelope,
    ) -> Result<CasOutcome, StoreError> {
        self.require_execution(execution_id)?;
        let attempted_bytes = checkpoint.encoded_len().map_err(|error| {
            StoreError::new(
                StoreErrorKind::Other,
                format!("measure proposed pilot checkpoint: {error}"),
            )
        })?;
        let attempted_checkpoint = checkpoint.clone();
        {
            let mut measurements = self
                .measurements
                .lock()
                .expect("pilot checkpoint measurements lock poisoned");
            measurements.write_attempts = measurements.write_attempts.saturating_add(1);
        }
        let result = self
            .inner
            .compare_and_swap(execution_id, expected, checkpoint)
            .await;
        let result_name = match &result {
            Ok(CasOutcome::Accepted(_)) => {
                let mut measurements = self
                    .measurements
                    .lock()
                    .expect("pilot checkpoint measurements lock poisoned");
                measurements.accepted_writes = measurements.accepted_writes.saturating_add(1);
                drop(measurements);
                self.record_authoritative_checkpoint(&attempted_checkpoint, attempted_bytes);
                "accepted"
            }
            Ok(CasOutcome::Conflict) => {
                let mut measurements = self
                    .measurements
                    .lock()
                    .expect("pilot checkpoint measurements lock poisoned");
                measurements.conflicts = measurements.conflicts.saturating_add(1);
                "conflict"
            }
            Ok(CasOutcome::OutcomeUnknown) => {
                let mut measurements = self
                    .measurements
                    .lock()
                    .expect("pilot checkpoint measurements lock poisoned");
                measurements.unknown_outcomes = measurements.unknown_outcomes.saturating_add(1);
                "outcome_unknown"
            }
            Err(_) => {
                let mut measurements = self
                    .measurements
                    .lock()
                    .expect("pilot checkpoint measurements lock poisoned");
                measurements.definite_failures = measurements.definite_failures.saturating_add(1);
                "definite_failure"
            }
        };
        let snapshot = self.measurements();
        self.collector.push(PilotCheckpointEvent {
            sequence: snapshot.write_attempts,
            execution_id,
            result: match &result {
                Ok(CasOutcome::Accepted(_)) => PilotCheckpointEventResult::Accepted,
                Ok(CasOutcome::Conflict) => PilotCheckpointEventResult::Conflict,
                Ok(CasOutcome::OutcomeUnknown) => PilotCheckpointEventResult::OutcomeUnknown,
                Err(_) => PilotCheckpointEventResult::DefiniteFailure,
            },
            attempted_checkpoint_bytes: attempted_bytes,
            authoritative_checkpoint_bytes: matches!(result, Ok(CasOutcome::Accepted(_)))
                .then_some(attempted_bytes),
            boundary: None,
        });
        info!(
            execution_id = %execution_id,
            operation = "compare_and_swap",
            result = result_name,
            attempted_checkpoint_bytes = attempted_bytes,
            authoritative = matches!(result, Ok(CasOutcome::Accepted(_))),
            "durable switchover checkpoint"
        );
        result
    }
}

// COMPLEXITY-BOUNDARY: pilot-store:end
#[cfg(test)]
mod durable_switchover_pilot_tests {
    use super::*;
    use kuberic_durable_execution::{
        ExactBytes, ExecutionContract, ExecutionSpec, InMemoryFault, ReloadReason, TerminalOutcome,
    };

    fn checkpoint(value: &[u8]) -> CheckpointEnvelope {
        CheckpointEnvelope::new(3, ExactBytes::new(value))
    }

    #[tokio::test]
    async fn measurements_distinguish_authoritative_and_unknown_bytes() {
        let execution_id = ExecutionId::from_bytes([9; 16]);
        let backend = kuberic_durable_execution::InMemoryCheckpointStore::new();
        let store = MeasuredPilotCheckpointStore::new(
            execution_id,
            PilotCheckpointStore::InMemory(backend.clone()),
        );
        let accepted = store
            .compare_and_swap(execution_id, None, checkpoint(b"first"))
            .await
            .unwrap();
        let CasOutcome::Accepted(revision) = accepted else {
            panic!("first write must be accepted");
        };
        store.correlate_host_outcome(&HostOutcome::WorkflowCompleted {
            outcome: TerminalOutcome::succeeded(ExactBytes::new(b"terminal")),
            revision: revision.clone(),
            boundary: PersistenceBoundary::Completion,
        });
        let accepted_bytes = store
            .load(execution_id)
            .await
            .unwrap()
            .unwrap()
            .checkpoint()
            .encoded_len()
            .unwrap();

        backend.fail_next_compare_and_swap(InMemoryFault::OutcomeUnknownWithoutApply);
        assert_eq!(
            store
                .compare_and_swap(
                    execution_id,
                    Some(revision),
                    checkpoint(b"unconfirmed-larger-payload")
                )
                .await
                .unwrap(),
            CasOutcome::OutcomeUnknown
        );
        store.correlate_host_outcome(&HostOutcome::ReloadRequired {
            boundary: PersistenceBoundary::Exposure,
            reason: ReloadReason::OutcomeUnknown,
        });
        let after_unknown = store.measurements();
        assert_eq!(after_unknown.write_attempts, 2);
        assert_eq!(after_unknown.accepted_writes, 1);
        assert_eq!(after_unknown.unknown_outcomes, 1);
        assert_eq!(
            after_unknown.latest_authoritative_checkpoint_bytes,
            Some(accepted_bytes)
        );

        let unchanged = store.load(execution_id).await.unwrap().unwrap();
        assert_eq!(
            store.measurements().latest_authoritative_checkpoint_bytes,
            Some(accepted_bytes)
        );

        assert_eq!(
            store
                .compare_and_swap(
                    execution_id,
                    Some(StorageRevision::new("stale").unwrap()),
                    checkpoint(b"conflict")
                )
                .await
                .unwrap(),
            CasOutcome::Conflict
        );
        store.correlate_host_outcome(&HostOutcome::ReloadRequired {
            boundary: PersistenceBoundary::Schedule,
            reason: ReloadReason::Conflict,
        });

        backend.fail_next_compare_and_swap(InMemoryFault::OutcomeUnknownAfterApply);
        let applied = checkpoint(b"applied-unknown");
        let applied_bytes = applied.encoded_len().unwrap();
        assert_eq!(
            store
                .compare_and_swap(execution_id, Some(unchanged.revision().clone()), applied)
                .await
                .unwrap(),
            CasOutcome::OutcomeUnknown
        );
        store.correlate_host_outcome(&HostOutcome::ReloadRequired {
            boundary: PersistenceBoundary::Observation,
            reason: ReloadReason::OutcomeUnknown,
        });
        assert_eq!(
            store.measurements().latest_authoritative_checkpoint_bytes,
            Some(accepted_bytes)
        );
        store.load(execution_id).await.unwrap();
        let final_measurements = store.measurements();
        assert_eq!(final_measurements.write_attempts, 4);
        assert_eq!(final_measurements.accepted_writes, 1);
        assert_eq!(final_measurements.conflicts, 1);
        assert_eq!(final_measurements.unknown_outcomes, 2);
        assert_eq!(
            final_measurements.latest_authoritative_checkpoint_bytes,
            Some(applied_bytes)
        );
        let events = store.collector().events();
        assert_eq!(events.len(), 4);
        assert_eq!(events[0].boundary, Some(PersistenceBoundary::Completion));
        assert_eq!(events[1].boundary, Some(PersistenceBoundary::Exposure));
        assert_eq!(events[2].boundary, Some(PersistenceBoundary::Schedule));
        assert_eq!(events[3].boundary, Some(PersistenceBoundary::Observation));
    }

    #[tokio::test]
    async fn measurements_split_active_terminal_and_boundary_count() {
        let execution_id = ExecutionId::from_bytes([10; 16]);
        let execution = ExecutionSpec::new(execution_id, ExactBytes::new(b"workflow"), 128);
        let contract = ExecutionContract::new(execution, 100_000);
        let backend = kuberic_durable_execution::InMemoryCheckpointStore::new();
        let store = MeasuredPilotCheckpointStore::new(
            execution_id,
            PilotCheckpointStore::InMemory(backend.clone()),
        );
        let active =
            CheckpointEnvelope::encode(&CheckpointPayload::active(contract.clone(), Vec::new()))
                .unwrap();
        let active_bytes = active.encoded_len().unwrap();
        let CasOutcome::Accepted(revision) = store
            .compare_and_swap(execution_id, None, active)
            .await
            .unwrap()
        else {
            panic!("active checkpoint was not accepted");
        };
        let terminal = CheckpointEnvelope::encode(&CheckpointPayload::terminal(
            contract,
            TerminalOutcome::succeeded(ExactBytes::new(b"done")),
            12,
        ))
        .unwrap();
        let terminal_bytes = terminal.encoded_len().unwrap();
        assert!(matches!(
            store
                .compare_and_swap(execution_id, Some(revision), terminal)
                .await
                .unwrap(),
            CasOutcome::Accepted(_)
        ));

        let measurements = store.measurements();
        assert_eq!(measurements.maximum_active_checkpoint_bytes, active_bytes);
        assert_eq!(
            measurements.latest_terminal_checkpoint_bytes,
            Some(terminal_bytes)
        );
        assert_eq!(
            measurements.maximum_terminal_checkpoint_bytes,
            terminal_bytes
        );
        assert_eq!(measurements.completed_activity_count, Some(12));
        assert_eq!(measurements.completed_external_effect_count, None);
        assert_eq!(measurements.completed_passive_observation_count, None);

        let restarted = MeasuredPilotCheckpointStore::new(
            execution_id,
            PilotCheckpointStore::InMemory(backend),
        );
        restarted.load(execution_id).await.unwrap();
        let reloaded = restarted.measurements();
        assert_eq!(reloaded.completed_activity_count, Some(12));
        assert_eq!(
            reloaded.latest_terminal_checkpoint_bytes,
            Some(terminal_bytes)
        );
        assert_eq!(reloaded.completed_external_effect_count, None);
        assert_eq!(reloaded.completed_passive_observation_count, None);
    }

    #[tokio::test]
    async fn production_event_history_is_bounded() {
        let execution_id = ExecutionId::from_bytes([10; 16]);
        let backend = kuberic_durable_execution::InMemoryCheckpointStore::new();
        let store = MeasuredPilotCheckpointStore::new(
            execution_id,
            PilotCheckpointStore::InMemory(backend.clone()),
        );
        for sequence in 1..=70_u64 {
            backend.fail_next_compare_and_swap(InMemoryFault::ConflictWithoutApply);
            assert_eq!(
                store
                    .compare_and_swap(execution_id, None, checkpoint(b"bounded"))
                    .await
                    .unwrap(),
                CasOutcome::Conflict
            );
            assert_eq!(store.measurements().write_attempts, sequence);
        }
        let events = store.collector().events();
        assert_eq!(events.len(), MAX_RECENT_CHECKPOINT_EVENTS);
        assert_eq!(events.first().unwrap().sequence, 7);
        assert_eq!(events.last().unwrap().sequence, 70);
    }
}
