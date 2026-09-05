use async_trait::async_trait;
use futures::{FutureExt, future::poll_fn, join, task::AtomicWaker};
use kuberic_durable_execution::{
    ActivityName, ActivityObservation, ActivitySequence, AttemptId, CasOutcome, CheckpointEnvelope,
    CheckpointPayload, CheckpointStore, DispatchPermit, DurableHost, ExactBytes, ExecutionId,
    HostEpoch, HostOutcome, InMemoryCheckpointStore, InMemoryFault, LogicalActivityId,
    Nondeterminism, ObservationRejection, PersistenceBoundary, ReloadReason, StorageRevision,
    StoreError, StoreErrorKind, StoredCheckpoint, Workflow, WorkflowContext,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ScenarioId {
    RestartBeforeSchedulePersistence,
    RestartAfterSchedulePersistence,
    RestartAfterDispatchExposure,
    LostReplyFollowedByObservation,
    DuplicateSchedulePollAndConflict,
    DuplicateExposurePollAndConflict,
    ChangedActivityOrder,
    ChangedActivityName,
    ChangedExactInput,
    RefreshedAttemptStableLogicalIdentity,
    AmbiguityQuarantine,
    DeterministicCompletedReplay,
    UnsupportedCheckpointFormat,
    ScheduleOutcomeUnknownAfterApply,
    ExposureOutcomeUnknownAfterApply,
    ObservationOutcomeUnknownAfterApply,
    MismatchedObservation,
    CompetingObservations,
    QuarantineResolution,
    QuarantineBlocksAllProgress,
    LoadAbsenceAndProviderFailures,
    OpaqueStorageRevisions,
    OutcomeUnknownApplyStateHidden,
}

impl ScenarioId {
    pub const ALL: [Self; 23] = [
        Self::RestartBeforeSchedulePersistence,
        Self::RestartAfterSchedulePersistence,
        Self::RestartAfterDispatchExposure,
        Self::LostReplyFollowedByObservation,
        Self::DuplicateSchedulePollAndConflict,
        Self::DuplicateExposurePollAndConflict,
        Self::ChangedActivityOrder,
        Self::ChangedActivityName,
        Self::ChangedExactInput,
        Self::RefreshedAttemptStableLogicalIdentity,
        Self::AmbiguityQuarantine,
        Self::DeterministicCompletedReplay,
        Self::UnsupportedCheckpointFormat,
        Self::ScheduleOutcomeUnknownAfterApply,
        Self::ExposureOutcomeUnknownAfterApply,
        Self::ObservationOutcomeUnknownAfterApply,
        Self::MismatchedObservation,
        Self::CompetingObservations,
        Self::QuarantineResolution,
        Self::QuarantineBlocksAllProgress,
        Self::LoadAbsenceAndProviderFailures,
        Self::OpaqueStorageRevisions,
        Self::OutcomeUnknownApplyStateHidden,
    ];

    pub const fn stable_id(self) -> &'static str {
        match self {
            Self::RestartBeforeSchedulePersistence => "FR-013-01",
            Self::RestartAfterSchedulePersistence => "FR-013-02",
            Self::RestartAfterDispatchExposure => "FR-013-03",
            Self::LostReplyFollowedByObservation => "FR-013-04",
            Self::DuplicateSchedulePollAndConflict => "FR-013-05",
            Self::DuplicateExposurePollAndConflict => "FR-013-06",
            Self::ChangedActivityOrder => "FR-013-07",
            Self::ChangedActivityName => "FR-013-08",
            Self::ChangedExactInput => "FR-013-09",
            Self::RefreshedAttemptStableLogicalIdentity => "FR-013-10",
            Self::AmbiguityQuarantine => "FR-013-11",
            Self::DeterministicCompletedReplay => "FR-013-12",
            Self::UnsupportedCheckpointFormat => "FR-013-13",
            Self::ScheduleOutcomeUnknownAfterApply => "FR-013-14",
            Self::ExposureOutcomeUnknownAfterApply => "FR-013-15",
            Self::ObservationOutcomeUnknownAfterApply => "FR-013-16",
            Self::MismatchedObservation => "FR-013-17",
            Self::CompetingObservations => "FR-013-18",
            Self::QuarantineResolution => "FR-013-19",
            Self::QuarantineBlocksAllProgress => "FR-013-20",
            Self::LoadAbsenceAndProviderFailures => "FR-013-21",
            Self::OpaqueStorageRevisions => "FR-013-22",
            Self::OutcomeUnknownApplyStateHidden => "FR-013-23",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AssertionEvidence {
    pub assertion: &'static str,
    pub passed: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScenarioEvidence {
    pub id: ScenarioId,
    pub setup: &'static str,
    pub assertions: Vec<AssertionEvidence>,
}

impl ScenarioEvidence {
    pub fn passed(&self) -> bool {
        self.assertions.iter().all(|assertion| assertion.passed)
    }

    fn new(
        id: ScenarioId,
        setup: &'static str,
        assertions: impl IntoIterator<Item = (&'static str, bool)>,
    ) -> Self {
        Self {
            id,
            setup,
            assertions: assertions
                .into_iter()
                .map(|(assertion, passed)| AssertionEvidence { assertion, passed })
                .collect(),
        }
    }
}

pub async fn run_conformance_matrix() -> Vec<ScenarioEvidence> {
    let mut evidence = Vec::with_capacity(ScenarioId::ALL.len());
    for id in ScenarioId::ALL {
        evidence.push(run_scenario(id).await);
    }
    evidence
}

async fn run_scenario(id: ScenarioId) -> ScenarioEvidence {
    AssertUnwindSafe(run_scenario_inner(id))
        .catch_unwind()
        .await
        .unwrap_or_else(|_| {
            ScenarioEvidence::new(
                id,
                "scenario setup or execution panicked",
                [("scenario completed and emitted structured evidence", false)],
            )
        })
}

async fn run_scenario_inner(id: ScenarioId) -> ScenarioEvidence {
    match id {
        ScenarioId::RestartBeforeSchedulePersistence => restart_before_schedule(id).await,
        ScenarioId::RestartAfterSchedulePersistence => restart_after_schedule(id).await,
        ScenarioId::RestartAfterDispatchExposure => restart_after_exposure(id).await,
        ScenarioId::LostReplyFollowedByObservation => lost_reply_then_observation(id).await,
        ScenarioId::DuplicateSchedulePollAndConflict => schedule_conflict(id).await,
        ScenarioId::DuplicateExposurePollAndConflict => exposure_conflict(id).await,
        ScenarioId::ChangedActivityOrder => changed_order(id).await,
        ScenarioId::ChangedActivityName => changed_name(id).await,
        ScenarioId::ChangedExactInput => changed_input(id).await,
        ScenarioId::RefreshedAttemptStableLogicalIdentity => refreshed_attempt(id).await,
        ScenarioId::AmbiguityQuarantine => quarantine(id).await,
        ScenarioId::DeterministicCompletedReplay => completed_replay(id).await,
        ScenarioId::UnsupportedCheckpointFormat => unsupported_format(id).await,
        ScenarioId::ScheduleOutcomeUnknownAfterApply => {
            schedule_outcome_unknown_after_apply(id).await
        }
        ScenarioId::ExposureOutcomeUnknownAfterApply => {
            exposure_outcome_unknown_after_apply(id).await
        }
        ScenarioId::ObservationOutcomeUnknownAfterApply => {
            observation_outcome_unknown_after_apply(id).await
        }
        ScenarioId::MismatchedObservation => mismatched_observation(id).await,
        ScenarioId::CompetingObservations => competing_observations(id).await,
        ScenarioId::QuarantineResolution => quarantine_resolution(id).await,
        ScenarioId::QuarantineBlocksAllProgress => quarantine_blocks_progress(id).await,
        ScenarioId::LoadAbsenceAndProviderFailures => load_absence_and_provider_failures(id).await,
        ScenarioId::OpaqueStorageRevisions => opaque_storage_revisions(id).await,
        ScenarioId::OutcomeUnknownApplyStateHidden => outcome_unknown_apply_state_hidden(id).await,
    }
}

#[derive(Default)]
struct AsyncBarrier {
    arrivals: AtomicUsize,
    waiter: AtomicWaker,
}

impl AsyncBarrier {
    async fn wait(&self) {
        let mut arrived = false;
        poll_fn(|context| {
            if !arrived {
                arrived = true;
                if self.arrivals.fetch_add(1, Ordering::AcqRel) + 1 == 2 {
                    self.waiter.wake();
                    return Poll::Ready(());
                }
            }
            if self.arrivals.load(Ordering::Acquire) >= 2 {
                Poll::Ready(())
            } else {
                self.waiter.register(context.waker());
                if self.arrivals.load(Ordering::Acquire) >= 2 {
                    Poll::Ready(())
                } else {
                    Poll::Pending
                }
            }
        })
        .await;
    }
}

#[derive(Clone)]
struct ContendedStore {
    inner: InMemoryCheckpointStore,
    compare_barrier: Arc<AsyncBarrier>,
}

impl ContendedStore {
    fn pair(inner: InMemoryCheckpointStore) -> (Self, Self) {
        let compare_barrier = Arc::new(AsyncBarrier::default());
        let first = Self {
            inner: inner.clone(),
            compare_barrier: compare_barrier.clone(),
        };
        let second = Self {
            inner,
            compare_barrier,
        };
        (first, second)
    }
}

#[async_trait(?Send)]
impl CheckpointStore for ContendedStore {
    async fn load(
        &self,
        execution_id: ExecutionId,
    ) -> Result<Option<StoredCheckpoint>, StoreError> {
        self.inner.load(execution_id).await
    }

    async fn compare_and_swap(
        &self,
        execution_id: ExecutionId,
        expected: Option<StorageRevision>,
        checkpoint: CheckpointEnvelope,
    ) -> Result<CasOutcome, StoreError> {
        self.compare_barrier.wait().await;
        self.inner
            .compare_and_swap(execution_id, expected, checkpoint)
            .await
    }
}

async fn contending_turns(
    store: InMemoryCheckpointStore,
    workflow: LinearWorkflow,
    execution_id: ExecutionId,
    input: ExactBytes,
) -> [HostOutcome; 2] {
    let (first_store, second_store) = ContendedStore::pair(store);
    let mut first_host = DurableHost::new(first_store, epoch(201));
    let mut second_host = DurableHost::new(second_store, epoch(202));
    let first_workflow = workflow.clone();
    let first_input = input.clone();
    let (first, second) = join!(
        first_host.turn(&first_workflow, execution_id, first_input),
        second_host.turn(&workflow, execution_id, input)
    );
    [first, second]
}

#[derive(Clone)]
pub struct LinearWorkflow {
    activities: Vec<(ActivityName, ExactBytes)>,
}

impl LinearWorkflow {
    pub fn one(name: &str, version: u32, input: &[u8]) -> Self {
        Self {
            activities: vec![(activity_name(name, version), bytes(input))],
        }
    }

    fn two() -> Self {
        Self {
            activities: vec![
                (activity_name("first", 1), bytes(b"A")),
                (activity_name("second", 1), bytes(b"B")),
            ],
        }
    }
}

#[async_trait(?Send)]
impl Workflow for LinearWorkflow {
    async fn run(&self, context: &mut WorkflowContext<'_>, _input: ExactBytes) -> ExactBytes {
        let mut result = Vec::new();
        for (name, input) in &self.activities {
            result.extend(
                context
                    .activity(name.clone(), input.clone())
                    .await
                    .as_slice(),
            );
        }
        ExactBytes::new(result)
    }
}

fn execution(value: u8) -> ExecutionId {
    ExecutionId::from_bytes([value; 16])
}

fn epoch(value: u8) -> HostEpoch {
    HostEpoch::from_bytes([value; 16])
}

fn bytes(value: &[u8]) -> ExactBytes {
    ExactBytes::new(value)
}

fn activity_name(value: &str, version: u32) -> ActivityName {
    ActivityName::new(value, version).unwrap()
}

fn host(store: InMemoryCheckpointStore, epoch_value: u8) -> DurableHost<InMemoryCheckpointStore> {
    DurableHost::new(store, epoch(epoch_value))
}

async fn schedule(
    host: &mut DurableHost<InMemoryCheckpointStore>,
    workflow: &LinearWorkflow,
    execution_id: ExecutionId,
    input: &ExactBytes,
) -> Option<LogicalActivityId> {
    match host.turn(workflow, execution_id, input.clone()).await {
        HostOutcome::ScheduleAccepted { activity, .. } => Some(activity),
        _ => None,
    }
}

async fn expose(
    host: &mut DurableHost<InMemoryCheckpointStore>,
    workflow: &LinearWorkflow,
    execution_id: ExecutionId,
    input: &ExactBytes,
) -> Option<DispatchPermit> {
    match host.turn(workflow, execution_id, input.clone()).await {
        HostOutcome::DispatchPermitted { permit, .. } => Some(permit),
        _ => None,
    }
}

async fn prepared_one(
    value: u8,
) -> (
    InMemoryCheckpointStore,
    DurableHost<InMemoryCheckpointStore>,
    LinearWorkflow,
    ExecutionId,
    ExactBytes,
    LogicalActivityId,
    DispatchPermit,
) {
    let store = InMemoryCheckpointStore::new();
    let workflow = LinearWorkflow::one("effect", 1, b"activity");
    let execution_id = execution(value);
    let input = bytes(b"workflow");
    let mut durable_host = host(store.clone(), value);
    let logical = schedule(&mut durable_host, &workflow, execution_id, &input)
        .await
        .unwrap();
    let permit = expose(&mut durable_host, &workflow, execution_id, &input)
        .await
        .unwrap();
    (
        store,
        durable_host,
        workflow,
        execution_id,
        input,
        logical,
        permit,
    )
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct SyntheticEffectCall {
    activity: LogicalActivityId,
    attempt_id: AttemptId,
}

#[derive(Default)]
struct SyntheticEffect {
    calls: Vec<SyntheticEffectCall>,
}

impl SyntheticEffect {
    fn invoke(&mut self, permit: DispatchPermit) -> ExactBytes {
        self.calls.push(SyntheticEffectCall {
            activity: permit.activity().clone(),
            attempt_id: permit.attempt_id(),
        });
        bytes(b"effect-result")
    }
}

async fn restart_before_schedule(id: ScenarioId) -> ScenarioEvidence {
    let store = InMemoryCheckpointStore::new();
    let workflow = LinearWorkflow::one("effect", 1, b"A");
    let execution_id = execution(1);
    let input = bytes(b"workflow");
    let mut first = host(store.clone(), 1);
    store.fail_next_compare_and_swap(InMemoryFault::FailBeforeRequest(
        StoreErrorKind::Unavailable,
    ));
    let rejected = first.turn(&workflow, execution_id, input.clone()).await;
    let absent = store
        .load(execution_id)
        .await
        .expect("scenario load must not fail")
        .is_none();
    let mut restarted = host(store.clone(), 2);
    let accepted = restarted.turn(&workflow, execution_id, input).await;
    ScenarioEvidence::new(
        id,
        "fail schedule before request, discard host, and replay",
        [
            (
                "rejected schedule requires reload and grants no permit",
                matches!(
                    rejected,
                    HostOutcome::StoreFailed {
                        operation: kuberic_durable_execution::StoreOperation::CompareAndSwap(
                            PersistenceBoundary::Schedule
                        ),
                        ..
                    }
                ),
            ),
            ("no checkpoint was persisted", absent),
            (
                "restarted host accepts the same schedule",
                matches!(accepted, HostOutcome::ScheduleAccepted { .. }),
            ),
        ],
    )
}

async fn restart_after_schedule(id: ScenarioId) -> ScenarioEvidence {
    let store = InMemoryCheckpointStore::new();
    let workflow = LinearWorkflow::one("effect", 1, b"A");
    let execution_id = execution(2);
    let input = bytes(b"workflow");
    let mut first = host(store.clone(), 1);
    let (logical, schedule_revision) = match first
        .turn(&workflow, execution_id, input.clone())
        .await
    {
        HostOutcome::ScheduleAccepted { activity, revision } => (Some(activity), Some(revision)),
        _ => (None, None),
    };
    let mut restarted = host(store, 2);
    let (permit, exposure_revision) =
        match restarted.turn(&workflow, execution_id, input.clone()).await {
            HostOutcome::DispatchPermitted { permit, revision } => (Some(permit), Some(revision)),
            _ => (None, None),
        };
    ScenarioEvidence::new(
        id,
        "persist schedule, discard host, and prepare exposure in a new epoch",
        [
            ("schedule was accepted", logical.is_some()),
            (
                "restart grants a permit only after exposure acceptance",
                permit.is_some(),
            ),
            (
                "schedule and exposure are separate accepted revisions",
                schedule_revision
                    .zip(exposure_revision)
                    .is_some_and(|(schedule, exposure)| schedule != exposure),
            ),
            (
                "logical identity survives restart",
                permit
                    .as_ref()
                    .zip(logical.as_ref())
                    .is_some_and(|(permit, logical)| permit.activity() == logical),
            ),
        ],
    )
}

async fn restart_after_exposure(id: ScenarioId) -> ScenarioEvidence {
    let (store, _, workflow, execution_id, input, logical, permit) = prepared_one(3).await;
    let before = store
        .load(execution_id)
        .await
        .expect("scenario load must not fail");
    let mut restarted = host(store.clone(), 9);
    let outcome = restarted.turn(&workflow, execution_id, input).await;
    let after = store
        .load(execution_id)
        .await
        .expect("scenario load must not fail");
    ScenarioEvidence::new(
        id,
        "persist dispatch exposure, discard host, and replay unresolved work",
        [
            (
                "restart enters quarantine with the same logical activity",
                matches!(
                    &outcome,
                    HostOutcome::Quarantined { activity, .. } if activity == &logical
                ),
            ),
            (
                "quarantine retains the persisted winning attempt",
                matches!(
                    outcome,
                    HostOutcome::Quarantined { attempt_id, .. }
                        if attempt_id == permit.attempt_id()
                ),
            ),
            ("quarantine performs no persistence", before == after),
        ],
    )
}

async fn lost_reply_then_observation(id: ScenarioId) -> ScenarioEvidence {
    let (store, _, workflow, execution_id, input, logical, permit) = prepared_one(4).await;
    let expected_attempt = permit.attempt_id();
    let mut effect = SyntheticEffect::default();
    let lost_effect_reply = effect.invoke(permit);
    drop(lost_effect_reply);
    let mut restarted = host(store, 40);
    let quarantined = restarted.turn(&workflow, execution_id, input.clone()).await;
    let observed = restarted
        .observe(
            execution_id,
            &input,
            ActivityObservation::new(logical.clone(), bytes(b"effect-result")),
        )
        .await;
    let completed = restarted.turn(&workflow, execution_id, input).await;
    ScenarioEvidence::new(
        id,
        "invoke one permitted external effect, discard its reply, restart into quarantine, then inject an authoritative observation",
        [
            (
                "exactly one permitted external effect was invoked",
                effect.calls
                    == [SyntheticEffectCall {
                        activity: logical.clone(),
                        attempt_id: expected_attempt,
                    }],
            ),
            (
                "discarded effect reply leaves the same logical attempt quarantined",
                matches!(
                    quarantined,
                    HostOutcome::Quarantined {
                        activity,
                        attempt_id
                    } if activity == logical && attempt_id == expected_attempt
                ),
            ),
            (
                "authoritative observation is accepted",
                matches!(observed, HostOutcome::ObservationAccepted { .. }),
            ),
            (
                "observed result replays to workflow completion",
                matches!(
                    completed,
                    HostOutcome::WorkflowCompleted { result }
                        if result == bytes(b"effect-result")
                ),
            ),
        ],
    )
}

async fn schedule_conflict(id: ScenarioId) -> ScenarioEvidence {
    let store = InMemoryCheckpointStore::new();
    let workflow = LinearWorkflow::one("effect", 1, b"A");
    let execution_id = execution(5);
    let input = bytes(b"workflow");
    let outcomes =
        contending_turns(store.clone(), workflow.clone(), execution_id, input.clone()).await;
    let accepted = outcomes
        .iter()
        .filter(|outcome| matches!(outcome, HostOutcome::ScheduleAccepted { .. }))
        .count();
    let conflicts = outcomes
        .iter()
        .filter(|outcome| {
            matches!(
                outcome,
                HostOutcome::ReloadRequired {
                    boundary: PersistenceBoundary::Schedule,
                    reason: ReloadReason::Conflict
                }
            )
        })
        .count();
    let permits = outcomes
        .iter()
        .filter(|outcome| matches!(outcome, HostOutcome::DispatchPermitted { .. }))
        .count();
    let mut after_race = host(store, 5);
    let exposed = after_race.turn(&workflow, execution_id, input).await;
    ScenarioEvidence::new(
        id,
        "race two hosts loaded at the missing-checkpoint schedule revision",
        [
            (
                "atomic CAS accepts exactly one schedule contender",
                accepted == 1 && conflicts == 1,
            ),
            (
                "schedule contenders receive no dispatch permit",
                permits == 0,
            ),
            (
                "dispatch is possible only on the later exposure turn",
                matches!(exposed, HostOutcome::DispatchPermitted { .. }),
            ),
        ],
    )
}

async fn exposure_conflict(id: ScenarioId) -> ScenarioEvidence {
    let store = InMemoryCheckpointStore::new();
    let workflow = LinearWorkflow::one("effect", 1, b"A");
    let execution_id = execution(6);
    let input = bytes(b"workflow");
    let mut scheduling_host = host(store.clone(), 6);
    let logical = schedule(&mut scheduling_host, &workflow, execution_id, &input)
        .await
        .unwrap();
    let outcomes =
        contending_turns(store.clone(), workflow.clone(), execution_id, input.clone()).await;
    let accepted_permits: Vec<_> = outcomes
        .iter()
        .filter_map(|outcome| match outcome {
            HostOutcome::DispatchPermitted { permit, .. } => Some(permit),
            _ => None,
        })
        .collect();
    let conflicts = outcomes
        .iter()
        .filter(|outcome| {
            matches!(
                outcome,
                HostOutcome::ReloadRequired {
                    boundary: PersistenceBoundary::Exposure,
                    reason: ReloadReason::Conflict
                }
            )
        })
        .count();
    let mut after_race = host(store, 6);
    let duplicate = after_race.turn(&workflow, execution_id, input).await;
    ScenarioEvidence::new(
        id,
        "race two hosts loaded at the same accepted schedule revision",
        [
            (
                "atomic CAS accepts exactly one exposure contender",
                accepted_permits.len() == 1 && conflicts == 1,
            ),
            (
                "the sole permit retains the scheduled logical identity",
                accepted_permits
                    .first()
                    .is_some_and(|permit| permit.activity() == &logical),
            ),
            (
                "a stale duplicate poll quarantines instead of redispatching",
                matches!(duplicate, HostOutcome::Quarantined { .. }),
            ),
        ],
    )
}

async fn changed_order(id: ScenarioId) -> ScenarioEvidence {
    let store = InMemoryCheckpointStore::new();
    let original = LinearWorkflow::two();
    let execution_id = execution(7);
    let input = bytes(b"workflow");
    let mut durable_host = host(store, 7);
    let logical = schedule(&mut durable_host, &original, execution_id, &input)
        .await
        .unwrap();
    expose(&mut durable_host, &original, execution_id, &input)
        .await
        .unwrap();
    durable_host
        .observe(
            execution_id,
            &input,
            ActivityObservation::new(logical, bytes(b"done")),
        )
        .await;
    let reordered = LinearWorkflow {
        activities: vec![
            (activity_name("second", 1), bytes(b"B")),
            (activity_name("first", 1), bytes(b"A")),
        ],
    };
    let outcome = durable_host.turn(&reordered, execution_id, input).await;
    ScenarioEvidence::new(
        id,
        "complete the first recorded activity and replay a reordered definition",
        [(
            "changed order is nondeterminism before any new dispatch",
            matches!(
                outcome,
                HostOutcome::Nondeterminism(Nondeterminism::ActivityMismatch { .. })
            ),
        )],
    )
}

async fn changed_name(id: ScenarioId) -> ScenarioEvidence {
    changed_definition(
        id,
        "replay a completed activity with a changed versioned name",
        LinearWorkflow::one("effect", 2, b"A"),
    )
    .await
}

async fn changed_input(id: ScenarioId) -> ScenarioEvidence {
    changed_definition(
        id,
        "replay a completed activity with changed exact bytes",
        LinearWorkflow::one("effect", 1, b"a"),
    )
    .await
}

async fn changed_definition(
    id: ScenarioId,
    setup: &'static str,
    changed: LinearWorkflow,
) -> ScenarioEvidence {
    let store = InMemoryCheckpointStore::new();
    let original = LinearWorkflow::one("effect", 1, b"A");
    let execution_id = execution(if id == ScenarioId::ChangedActivityName {
        8
    } else {
        9
    });
    let input = bytes(b"workflow");
    let mut durable_host = host(store, 8);
    let logical = schedule(&mut durable_host, &original, execution_id, &input)
        .await
        .unwrap();
    expose(&mut durable_host, &original, execution_id, &input)
        .await
        .unwrap();
    durable_host
        .observe(
            execution_id,
            &input,
            ActivityObservation::new(logical, bytes(b"done")),
        )
        .await;
    let outcome = durable_host.turn(&changed, execution_id, input).await;
    ScenarioEvidence::new(
        id,
        setup,
        [(
            "definition mismatch is nondeterminism before dispatch",
            matches!(
                outcome,
                HostOutcome::Nondeterminism(Nondeterminism::ActivityMismatch { .. })
            ),
        )],
    )
}

async fn refreshed_attempt(id: ScenarioId) -> ScenarioEvidence {
    let store = InMemoryCheckpointStore::new();
    let workflow = LinearWorkflow::one("effect", 1, b"A");
    let execution_id = execution(10);
    let input = bytes(b"workflow");
    let mut durable_host = host(store.clone(), 10);
    let logical = schedule(&mut durable_host, &workflow, execution_id, &input)
        .await
        .unwrap();
    store.fail_next_compare_and_swap(InMemoryFault::FailBeforeRequest(
        StoreErrorKind::Unavailable,
    ));
    let rejected = durable_host
        .turn(&workflow, execution_id, input.clone())
        .await;
    let accepted = durable_host.turn(&workflow, execution_id, input).await;
    ScenarioEvidence::new(
        id,
        "discard a pre-exposure attempt after rejection and prepare a fresh attempt",
        [
            (
                "discarded attempt confers no permit",
                matches!(
                    rejected,
                    HostOutcome::StoreFailed {
                        operation: kuberic_durable_execution::StoreOperation::CompareAndSwap(
                            PersistenceBoundary::Exposure
                        ),
                        ..
                    }
                ),
            ),
            (
                "accepted attempt was refreshed",
                matches!(
                    &accepted,
                    HostOutcome::DispatchPermitted { permit, .. }
                        if permit.attempt_id().counter() == 2
                ),
            ),
            (
                "logical identity is unchanged across attempts",
                matches!(
                    accepted,
                    HostOutcome::DispatchPermitted { permit, .. }
                        if permit.activity() == &logical
                ),
            ),
        ],
    )
}

async fn quarantine(id: ScenarioId) -> ScenarioEvidence {
    let (store, mut durable_host, workflow, execution_id, input, logical, _) =
        prepared_one(11).await;
    let changed = LinearWorkflow::one("changed-effect", 1, b"A");
    let before = store
        .load(execution_id)
        .await
        .expect("scenario load must not fail");
    let first = durable_host
        .turn(&workflow, execution_id, input.clone())
        .await;
    let changed_while_unresolved = durable_host
        .turn(&changed, execution_id, input.clone())
        .await;
    let second = durable_host
        .turn(&workflow, execution_id, input.clone())
        .await;
    let after = store
        .load(execution_id)
        .await
        .expect("scenario load must not fail");
    let observed = durable_host
        .observe(
            execution_id,
            &input,
            ActivityObservation::new(logical.clone(), bytes(b"resolved")),
        )
        .await;
    let changed_after_resolution = durable_host.turn(&changed, execution_id, input).await;
    ScenarioEvidence::new(
        id,
        "poll exposed unresolved work repeatedly and with a changed workflow definition",
        [
            (
                "first replay is quarantined",
                matches!(&first, HostOutcome::Quarantined { activity, .. } if activity == &logical),
            ),
            (
                "quarantine takes precedence over changed-definition matching",
                matches!(
                    &changed_while_unresolved,
                    HostOutcome::Quarantined { activity, .. } if activity == &logical
                ),
            ),
            (
                "duplicate replay remains quarantined without a permit",
                matches!(second, HostOutcome::Quarantined { .. }),
            ),
            ("quarantine does not mutate the checkpoint", before == after),
            (
                "after observation, the changed definition reports nondeterminism",
                matches!(observed, HostOutcome::ObservationAccepted { .. })
                    && matches!(
                        changed_after_resolution,
                        HostOutcome::Nondeterminism(Nondeterminism::ActivityMismatch { .. })
                    ),
            ),
        ],
    )
}

async fn completed_replay(id: ScenarioId) -> ScenarioEvidence {
    let (_, mut durable_host, workflow, execution_id, input, logical, _) = prepared_one(12).await;
    durable_host
        .observe(
            execution_id,
            &input,
            ActivityObservation::new(logical, bytes(b"recorded")),
        )
        .await;
    let first = durable_host
        .turn(&workflow, execution_id, input.clone())
        .await;
    let second = durable_host.turn(&workflow, execution_id, input).await;
    ScenarioEvidence::new(
        id,
        "replay the same completed checkpoint twice",
        [
            (
                "first replay returns the recorded result",
                matches!(
                    &first,
                    HostOutcome::WorkflowCompleted { result } if result == &bytes(b"recorded")
                ),
            ),
            ("second replay is semantically identical", first == second),
            (
                "neither completed replay grants dispatch",
                !matches!(first, HostOutcome::DispatchPermitted { .. })
                    && !matches!(second, HostOutcome::DispatchPermitted { .. }),
            ),
        ],
    )
}

async fn unsupported_format(id: ScenarioId) -> ScenarioEvidence {
    let store = InMemoryCheckpointStore::new();
    let workflow = LinearWorkflow::one("effect", 1, b"A");
    let execution_id = execution(13);
    let input = bytes(b"workflow");
    let valid = CheckpointEnvelope::encode(&CheckpointPayload::new(
        execution_id,
        input.clone(),
        Vec::new(),
    ))
    .unwrap();
    let unsupported = CheckpointEnvelope::new(999, valid.payload().clone());
    let stored = store
        .compare_and_swap(execution_id, None, unsupported)
        .await;
    let mut durable_host = host(store, 13);
    let outcome = durable_host.turn(&workflow, execution_id, input).await;
    ScenarioEvidence::new(
        id,
        "load a checkpoint with an unsupported envelope version",
        [
            (
                "test checkpoint was accepted by opaque storage",
                matches!(stored, Ok(CasOutcome::Accepted(_))),
            ),
            (
                "host rejects format before workflow or dispatch",
                matches!(outcome, HostOutcome::CheckpointRejected(_)),
            ),
        ],
    )
}

async fn schedule_outcome_unknown_after_apply(id: ScenarioId) -> ScenarioEvidence {
    let store = InMemoryCheckpointStore::new();
    let workflow = LinearWorkflow::one("effect", 1, b"A");
    let execution_id = execution(14);
    let input = bytes(b"workflow");
    let mut durable_host = host(store.clone(), 14);
    store.fail_next_compare_and_swap(InMemoryFault::OutcomeUnknownAfterApply);
    let lost = durable_host
        .turn(&workflow, execution_id, input.clone())
        .await;
    let loaded = store
        .load(execution_id)
        .await
        .expect("scenario load must not fail");
    let next = durable_host.turn(&workflow, execution_id, input).await;
    ScenarioEvidence::new(
        id,
        "lose the response after applying the schedule CAS",
        [
            (
                "uncertain response requires schedule reload with no permit",
                matches!(
                    lost,
                    HostOutcome::ReloadRequired {
                        boundary: PersistenceBoundary::Schedule,
                        reason: ReloadReason::OutcomeUnknown
                    }
                ),
            ),
            ("schedule was nevertheless persisted", loaded.is_some()),
            (
                "later exposure turn may receive permission",
                matches!(next, HostOutcome::DispatchPermitted { .. }),
            ),
        ],
    )
}

async fn exposure_outcome_unknown_after_apply(id: ScenarioId) -> ScenarioEvidence {
    let store = InMemoryCheckpointStore::new();
    let workflow = LinearWorkflow::one("effect", 1, b"A");
    let execution_id = execution(15);
    let input = bytes(b"workflow");
    let mut durable_host = host(store, 15);
    schedule(&mut durable_host, &workflow, execution_id, &input)
        .await
        .unwrap();
    durable_host
        .store()
        .fail_next_compare_and_swap(InMemoryFault::OutcomeUnknownAfterApply);
    let lost = durable_host
        .turn(&workflow, execution_id, input.clone())
        .await;
    let next = durable_host.turn(&workflow, execution_id, input).await;
    ScenarioEvidence::new(
        id,
        "lose the response after applying dispatch exposure",
        [
            (
                "uncertain exposure returns no permit",
                matches!(
                    lost,
                    HostOutcome::ReloadRequired {
                        boundary: PersistenceBoundary::Exposure,
                        reason: ReloadReason::OutcomeUnknown
                    }
                ),
            ),
            (
                "reload immediately quarantines unresolved work",
                matches!(next, HostOutcome::Quarantined { .. }),
            ),
        ],
    )
}

async fn observation_outcome_unknown_after_apply(id: ScenarioId) -> ScenarioEvidence {
    let (store, mut durable_host, workflow, execution_id, input, logical, _) =
        prepared_one(16).await;
    store.fail_next_compare_and_swap(InMemoryFault::OutcomeUnknownAfterApply);
    let lost = durable_host
        .observe(
            execution_id,
            &input,
            ActivityObservation::new(logical, bytes(b"observed")),
        )
        .await;
    let next = durable_host.turn(&workflow, execution_id, input).await;
    ScenarioEvidence::new(
        id,
        "lose the response after applying an authoritative observation",
        [
            (
                "uncertain observation requires reload",
                matches!(
                    lost,
                    HostOutcome::ReloadRequired {
                        boundary: PersistenceBoundary::Observation,
                        reason: ReloadReason::OutcomeUnknown
                    }
                ),
            ),
            (
                "reload replays the applied observed result",
                matches!(
                    next,
                    HostOutcome::WorkflowCompleted { result } if result == bytes(b"observed")
                ),
            ),
        ],
    )
}

async fn mismatched_observation(id: ScenarioId) -> ScenarioEvidence {
    let (_, mut durable_host, workflow, execution_id, input, logical, _) = prepared_one(17).await;
    let mismatched = LogicalActivityId::new(
        execution_id,
        ActivitySequence::new(0),
        activity_name("effect", 1),
        bytes(b"different"),
    );
    let rejected = durable_host
        .observe(
            execution_id,
            &input,
            ActivityObservation::new(mismatched, bytes(b"result")),
        )
        .await;
    let next = durable_host.turn(&workflow, execution_id, input).await;
    ScenarioEvidence::new(
        id,
        "inject a result for a different exact logical activity",
        [
            (
                "mismatched observation is rejected",
                matches!(
                    rejected,
                    HostOutcome::ObservationRejected(
                        ObservationRejection::LogicalActivityMismatch { expected, .. }
                    ) if expected == logical
                ),
            ),
            (
                "rejected observation leaves the activity quarantined",
                matches!(next, HostOutcome::Quarantined { .. }),
            ),
        ],
    )
}

async fn competing_observations(id: ScenarioId) -> ScenarioEvidence {
    let (store, _, workflow, execution_id, input, logical, _) = prepared_one(18).await;
    let (first_store, second_store) = ContendedStore::pair(store.clone());
    let logical_for_first = logical.clone();
    let input_for_first = input.clone();
    let replay_input = input.clone();
    let first_host = DurableHost::new(first_store, epoch(203));
    let second_host = DurableHost::new(second_store, epoch(204));
    let first_result = bytes(b"first");
    let second_result = bytes(b"second");
    let (first_outcome, second_outcome) = join!(
        first_host.observe(
            execution_id,
            &input_for_first,
            ActivityObservation::new(logical_for_first, first_result.clone()),
        ),
        second_host.observe(
            execution_id,
            &input,
            ActivityObservation::new(logical, second_result.clone()),
        )
    );
    let observations = [
        (first_result, first_outcome),
        (second_result, second_outcome),
    ];
    let accepted_results: Vec<_> = observations
        .iter()
        .filter_map(|(result, outcome)| {
            matches!(outcome, HostOutcome::ObservationAccepted { .. }).then_some(result)
        })
        .collect();
    let conflicts = observations
        .iter()
        .filter(|(_, outcome)| {
            matches!(
                outcome,
                HostOutcome::ReloadRequired {
                    boundary: PersistenceBoundary::Observation,
                    reason: ReloadReason::Conflict
                }
            )
        })
        .count();
    let mut durable_host = host(store, 18);
    let completed = durable_host
        .turn(&workflow, execution_id, replay_input)
        .await;
    ScenarioEvidence::new(
        id,
        "race two observations loaded from the same exposed revision",
        [
            (
                "atomic CAS accepts exactly one competing observation",
                accepted_results.len() == 1 && conflicts == 1,
            ),
            (
                "the accepted observation remains authoritative",
                matches!(
                    completed,
                    HostOutcome::WorkflowCompleted { result }
                        if accepted_results.first().is_some_and(|accepted| &result == *accepted)
                ),
            ),
        ],
    )
}

async fn quarantine_resolution(id: ScenarioId) -> ScenarioEvidence {
    let (store, mut durable_host, workflow, execution_id, input, logical, permit) =
        prepared_one(19).await;
    let quarantined = durable_host
        .turn(&workflow, execution_id, input.clone())
        .await;
    let accepted = durable_host
        .observe(
            execution_id,
            &input,
            ActivityObservation::new(logical.clone(), bytes(b"resolved")),
        )
        .await;
    let before_stale = store
        .load(execution_id)
        .await
        .expect("scenario load must not fail");
    let stale = durable_host
        .observe(
            execution_id,
            &input,
            ActivityObservation::new(logical.clone(), bytes(b"stale")),
        )
        .await;
    let stale_left_checkpoint_unchanged = before_stale
        == store
            .load(execution_id)
            .await
            .expect("scenario load must not fail");
    let completed = durable_host.turn(&workflow, execution_id, input).await;
    ScenarioEvidence::new(
        id,
        "resolve quarantined work only through the public observation API",
        [
            (
                "quarantine retains logical and attempt identity",
                matches!(
                    quarantined,
                    HostOutcome::Quarantined {
                        activity,
                        attempt_id
                    } if activity == logical && attempt_id == permit.attempt_id()
                ),
            ),
            (
                "public observation resolves the same logical activity",
                matches!(
                    accepted,
                    HostOutcome::ObservationAccepted { activity, .. } if activity == logical
                ),
            ),
            (
                "a stale observation is rejected without changing the completed record",
                matches!(
                    stale,
                    HostOutcome::ObservationRejected(ObservationRejection::ActivityNotExposed)
                ) && stale_left_checkpoint_unchanged,
            ),
            (
                "resolution replays without redispatch",
                matches!(completed, HostOutcome::WorkflowCompleted { .. }),
            ),
        ],
    )
}

async fn quarantine_blocks_progress(id: ScenarioId) -> ScenarioEvidence {
    let store = InMemoryCheckpointStore::new();
    let workflow = LinearWorkflow::two();
    let execution_id = execution(20);
    let input = bytes(b"workflow");
    let mut durable_host = host(store.clone(), 20);
    let first_logical = schedule(&mut durable_host, &workflow, execution_id, &input)
        .await
        .unwrap();
    expose(&mut durable_host, &workflow, execution_id, &input)
        .await
        .unwrap();
    let before = store
        .load(execution_id)
        .await
        .expect("scenario load must not fail");
    let blocked_one = durable_host
        .turn(&workflow, execution_id, input.clone())
        .await;
    let blocked_two = durable_host
        .turn(&workflow, execution_id, input.clone())
        .await;
    let still_same = before
        == store
            .load(execution_id)
            .await
            .expect("scenario load must not fail");
    durable_host
        .observe(
            execution_id,
            &input,
            ActivityObservation::new(first_logical, bytes(b"first-result")),
        )
        .await;
    let after_observation = durable_host.turn(&workflow, execution_id, input).await;
    ScenarioEvidence::new(
        id,
        "replay a two-activity workflow while the first exposure is unresolved",
        [
            (
                "quarantine returns neither redispatch nor another schedule",
                matches!(blocked_one, HostOutcome::Quarantined { .. })
                    && matches!(blocked_two, HostOutcome::Quarantined { .. }),
            ),
            (
                "no compensation or other mutation occurs in quarantine",
                still_same,
            ),
            (
                "the subsequent activity schedules only after observation",
                matches!(
                    after_observation,
                    HostOutcome::ScheduleAccepted { activity, .. }
                        if activity.sequence() == ActivitySequence::new(1)
                ),
            ),
        ],
    )
}

async fn load_absence_and_provider_failures(id: ScenarioId) -> ScenarioEvidence {
    let store = InMemoryCheckpointStore::new();
    let workflow = LinearWorkflow::one("effect", 1, b"A");
    let execution_id = execution(21);
    let input = bytes(b"workflow");
    let absence_is_distinct = store
        .load(execution_id)
        .await
        .expect("scenario load must not fail")
        .is_none();

    let mut kinds_preserved = true;
    let mut descriptions_preserved = true;
    let mut failures_granted_no_permit = true;
    for kind in [
        StoreErrorKind::Authorization,
        StoreErrorKind::Unavailable,
        StoreErrorKind::Timeout,
        StoreErrorKind::MalformedResponse,
        StoreErrorKind::Other,
    ] {
        store.fail_next_load(StoreError::new(kind, format!("{kind} provider detail")));
        let mut durable_host = host(store.clone(), 21);
        let outcome = durable_host
            .turn(&workflow, execution_id, input.clone())
            .await;
        kinds_preserved &= matches!(
            &outcome,
            HostOutcome::StoreFailed {
                operation: kuberic_durable_execution::StoreOperation::Load,
                error
            } if error.kind() == kind
        );
        descriptions_preserved &= matches!(
            &outcome,
            HostOutcome::StoreFailed { error, .. }
                if error.description().contains("provider detail")
        );
        failures_granted_no_permit &= !matches!(outcome, HostOutcome::DispatchPermitted { .. });
    }

    let (observation_store, durable_host, _, observed_execution, observed_input, logical, _) =
        prepared_one(121).await;
    observation_store.fail_next_load(StoreError::new(
        StoreErrorKind::Timeout,
        "observation load timed out",
    ));
    let observation_failure = durable_host
        .observe(
            observed_execution,
            &observed_input,
            ActivityObservation::new(logical, bytes(b"result")),
        )
        .await;

    ScenarioEvidence::new(
        id,
        "distinguish missing checkpoints from portable provider failures on async host paths",
        [
            ("missing checkpoint loads as absence", absence_is_distinct),
            (
                "all portable load failure classes reach the host unchanged",
                kinds_preserved,
            ),
            (
                "provider source descriptions remain available as text",
                descriptions_preserved,
            ),
            (
                "load failures grant no dispatch permit",
                failures_granted_no_permit,
            ),
            (
                "observation load failure is propagated without mutation",
                matches!(
                    observation_failure,
                    HostOutcome::StoreFailed {
                        operation: kuberic_durable_execution::StoreOperation::Load,
                        error
                    } if error.kind() == StoreErrorKind::Timeout
                ),
            ),
        ],
    )
}

async fn opaque_storage_revisions(id: ScenarioId) -> ScenarioEvidence {
    let store = InMemoryCheckpointStore::new();
    let workflow = LinearWorkflow::one("effect", 1, b"A");
    let execution_id = execution(22);
    let input = bytes(b"workflow");
    let mut durable_host = host(store, 22);
    let revision = match durable_host.turn(&workflow, execution_id, input).await {
        HostOutcome::ScheduleAccepted { revision, .. } => Some(revision),
        _ => None,
    };
    let round_trip = revision.as_ref().and_then(|revision| {
        StorageRevision::new(revision.as_str())
            .ok()
            .map(|decoded| decoded == *revision)
    });
    let empty = StorageRevision::new("").unwrap_err();

    ScenarioEvidence::new(
        id,
        "round-trip a nonnumeric opaque provider revision and reject an empty token",
        [
            (
                "in-memory revision is an opaque nonnumeric string",
                revision
                    .as_ref()
                    .is_some_and(|revision| revision.as_str().parse::<u64>().is_err()),
            ),
            (
                "revision equality survives provider string round-trip",
                round_trip == Some(true),
            ),
            (
                "empty provider revision is a malformed response",
                empty.kind() == StoreErrorKind::MalformedResponse,
            ),
        ],
    )
}

async fn outcome_unknown_apply_state_hidden(id: ScenarioId) -> ScenarioEvidence {
    let workflow = LinearWorkflow::one("effect", 1, b"A");
    let execution_id = execution(23);
    let input = bytes(b"workflow");

    let schedule_unapplied_store = InMemoryCheckpointStore::new();
    schedule_unapplied_store.fail_next_compare_and_swap(InMemoryFault::OutcomeUnknownWithoutApply);
    let mut schedule_unapplied_host = host(schedule_unapplied_store.clone(), 23);
    let schedule_unapplied = schedule_unapplied_host
        .turn(&workflow, execution_id, input.clone())
        .await;
    let schedule_remained_absent = schedule_unapplied_store
        .load(execution_id)
        .await
        .expect("scenario load must not fail")
        .is_none();

    let schedule_applied_store = InMemoryCheckpointStore::new();
    schedule_applied_store.fail_next_compare_and_swap(InMemoryFault::OutcomeUnknownAfterApply);
    let mut schedule_applied_host = host(schedule_applied_store.clone(), 24);
    let schedule_applied = schedule_applied_host
        .turn(&workflow, execution_id, input.clone())
        .await;
    let schedule_became_present = schedule_applied_store
        .load(execution_id)
        .await
        .expect("scenario load must not fail")
        .is_some();

    let exposure_unapplied_store = InMemoryCheckpointStore::new();
    let mut exposure_unapplied_host = host(exposure_unapplied_store.clone(), 25);
    schedule(
        &mut exposure_unapplied_host,
        &workflow,
        execution(123),
        &input,
    )
    .await
    .unwrap();
    let exposure_unapplied_before = exposure_unapplied_store
        .load(execution(123))
        .await
        .expect("scenario load must not fail");
    exposure_unapplied_store.fail_next_compare_and_swap(InMemoryFault::OutcomeUnknownWithoutApply);
    let exposure_unapplied = exposure_unapplied_host
        .turn(&workflow, execution(123), input.clone())
        .await;
    let exposure_unapplied_after = exposure_unapplied_store
        .load(execution(123))
        .await
        .expect("scenario load must not fail");

    let exposure_applied_store = InMemoryCheckpointStore::new();
    let mut exposure_applied_host = host(exposure_applied_store.clone(), 26);
    schedule(
        &mut exposure_applied_host,
        &workflow,
        execution(124),
        &input,
    )
    .await
    .unwrap();
    let exposure_applied_before = exposure_applied_store
        .load(execution(124))
        .await
        .expect("scenario load must not fail");
    exposure_applied_store.fail_next_compare_and_swap(InMemoryFault::OutcomeUnknownAfterApply);
    let exposure_applied = exposure_applied_host
        .turn(&workflow, execution(124), input.clone())
        .await;
    let exposure_applied_after = exposure_applied_store
        .load(execution(124))
        .await
        .expect("scenario load must not fail");

    let (
        observation_unapplied_store,
        observation_unapplied_host,
        _,
        observation_unapplied_execution,
        observation_unapplied_input,
        observation_unapplied_logical,
        _,
    ) = prepared_one(125).await;
    let observation_unapplied_before = observation_unapplied_store
        .load(observation_unapplied_execution)
        .await
        .expect("scenario load must not fail");
    observation_unapplied_store
        .fail_next_compare_and_swap(InMemoryFault::OutcomeUnknownWithoutApply);
    let observation_unapplied = observation_unapplied_host
        .observe(
            observation_unapplied_execution,
            &observation_unapplied_input,
            ActivityObservation::new(observation_unapplied_logical, bytes(b"result")),
        )
        .await;
    let observation_unapplied_after = observation_unapplied_store
        .load(observation_unapplied_execution)
        .await
        .expect("scenario load must not fail");

    let (
        observation_applied_store,
        observation_applied_host,
        _,
        observation_applied_execution,
        observation_applied_input,
        observation_applied_logical,
        _,
    ) = prepared_one(126).await;
    let observation_applied_before = observation_applied_store
        .load(observation_applied_execution)
        .await
        .expect("scenario load must not fail");
    observation_applied_store.fail_next_compare_and_swap(InMemoryFault::OutcomeUnknownAfterApply);
    let observation_applied = observation_applied_host
        .observe(
            observation_applied_execution,
            &observation_applied_input,
            ActivityObservation::new(observation_applied_logical, bytes(b"result")),
        )
        .await;
    let observation_applied_after = observation_applied_store
        .load(observation_applied_execution)
        .await
        .expect("scenario load must not fail");

    ScenarioEvidence::new(
        id,
        "inject the same outcome-unknown result with and without applying each CAS boundary",
        [
            (
                "schedule uncertainty hides whether the mutation applied",
                schedule_unapplied == schedule_applied
                    && matches!(
                        schedule_unapplied,
                        HostOutcome::ReloadRequired {
                            boundary: PersistenceBoundary::Schedule,
                            reason: ReloadReason::OutcomeUnknown
                        }
                    )
                    && schedule_remained_absent
                    && schedule_became_present,
            ),
            (
                "exposure uncertainty hides whether the mutation applied",
                exposure_unapplied == exposure_applied
                    && matches!(
                        exposure_unapplied,
                        HostOutcome::ReloadRequired {
                            boundary: PersistenceBoundary::Exposure,
                            reason: ReloadReason::OutcomeUnknown
                        }
                    )
                    && exposure_unapplied_before == exposure_unapplied_after
                    && exposure_applied_before != exposure_applied_after,
            ),
            (
                "observation uncertainty hides whether the mutation applied",
                observation_unapplied == observation_applied
                    && matches!(
                        observation_unapplied,
                        HostOutcome::ReloadRequired {
                            boundary: PersistenceBoundary::Observation,
                            reason: ReloadReason::OutcomeUnknown
                        }
                    )
                    && observation_unapplied_before == observation_unapplied_after
                    && observation_applied_before != observation_applied_after,
            ),
            (
                "no uncertain CAS outcome grants a dispatch permit",
                !matches!(schedule_applied, HostOutcome::DispatchPermitted { .. })
                    && !matches!(exposure_applied, HostOutcome::DispatchPermitted { .. })
                    && !matches!(observation_applied, HostOutcome::DispatchPermitted { .. }),
            ),
        ],
    )
}

use std::{
    panic::AssertUnwindSafe,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    task::Poll,
};
