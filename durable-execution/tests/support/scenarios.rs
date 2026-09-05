use async_trait::async_trait;
use kuberic_durable_execution::{
    ActivityName, ActivityObservation, ActivitySequence, CheckpointEnvelope, CheckpointPayload,
    CheckpointStore, CompareAndSwap, DispatchPermit, DurableHost, ExactBytes, ExecutionId,
    HostEpoch, HostOutcome, InMemoryCheckpointStore, InMemoryFault, LogicalActivityId,
    Nondeterminism, ObservationRejection, PersistenceBoundary, ReloadReason, Workflow,
    WorkflowContext,
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
    ScheduleResponseLostAfterApply,
    ExposureResponseLostAfterApply,
    ObservationResponseLostAfterApply,
    MismatchedObservation,
    CompetingObservations,
    QuarantineResolution,
    QuarantineBlocksAllProgress,
}

impl ScenarioId {
    pub const ALL: [Self; 20] = [
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
        Self::ScheduleResponseLostAfterApply,
        Self::ExposureResponseLostAfterApply,
        Self::ObservationResponseLostAfterApply,
        Self::MismatchedObservation,
        Self::CompetingObservations,
        Self::QuarantineResolution,
        Self::QuarantineBlocksAllProgress,
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
            Self::ScheduleResponseLostAfterApply => "FR-013-14",
            Self::ExposureResponseLostAfterApply => "FR-013-15",
            Self::ObservationResponseLostAfterApply => "FR-013-16",
            Self::MismatchedObservation => "FR-013-17",
            Self::CompetingObservations => "FR-013-18",
            Self::QuarantineResolution => "FR-013-19",
            Self::QuarantineBlocksAllProgress => "FR-013-20",
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

pub fn run_conformance_matrix() -> Vec<ScenarioEvidence> {
    ScenarioId::ALL.into_iter().map(run_scenario).collect()
}

fn run_scenario(id: ScenarioId) -> ScenarioEvidence {
    catch_unwind(AssertUnwindSafe(|| run_scenario_inner(id))).unwrap_or_else(|_| {
        ScenarioEvidence::new(
            id,
            "scenario setup or execution panicked",
            [("scenario completed and emitted structured evidence", false)],
        )
    })
}

fn run_scenario_inner(id: ScenarioId) -> ScenarioEvidence {
    match id {
        ScenarioId::RestartBeforeSchedulePersistence => restart_before_schedule(id),
        ScenarioId::RestartAfterSchedulePersistence => restart_after_schedule(id),
        ScenarioId::RestartAfterDispatchExposure => restart_after_exposure(id),
        ScenarioId::LostReplyFollowedByObservation => lost_reply_then_observation(id),
        ScenarioId::DuplicateSchedulePollAndConflict => schedule_conflict(id),
        ScenarioId::DuplicateExposurePollAndConflict => exposure_conflict(id),
        ScenarioId::ChangedActivityOrder => changed_order(id),
        ScenarioId::ChangedActivityName => changed_name(id),
        ScenarioId::ChangedExactInput => changed_input(id),
        ScenarioId::RefreshedAttemptStableLogicalIdentity => refreshed_attempt(id),
        ScenarioId::AmbiguityQuarantine => quarantine(id),
        ScenarioId::DeterministicCompletedReplay => completed_replay(id),
        ScenarioId::UnsupportedCheckpointFormat => unsupported_format(id),
        ScenarioId::ScheduleResponseLostAfterApply => schedule_response_lost(id),
        ScenarioId::ExposureResponseLostAfterApply => exposure_response_lost(id),
        ScenarioId::ObservationResponseLostAfterApply => observation_response_lost(id),
        ScenarioId::MismatchedObservation => mismatched_observation(id),
        ScenarioId::CompetingObservations => competing_observations(id),
        ScenarioId::QuarantineResolution => quarantine_resolution(id),
        ScenarioId::QuarantineBlocksAllProgress => quarantine_blocks_progress(id),
    }
}

#[derive(Clone)]
struct ContendedStore {
    inner: InMemoryCheckpointStore,
    compare_barrier: Arc<Barrier>,
}

impl ContendedStore {
    fn pair(inner: InMemoryCheckpointStore) -> (Self, Self) {
        let compare_barrier = Arc::new(Barrier::new(2));
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

impl CheckpointStore for ContendedStore {
    fn load(
        &self,
        execution_id: ExecutionId,
    ) -> Option<kuberic_durable_execution::StoredCheckpoint> {
        self.inner.load(execution_id)
    }

    fn compare_and_swap(
        &self,
        execution_id: ExecutionId,
        expected: Option<kuberic_durable_execution::StorageRevision>,
        checkpoint: CheckpointEnvelope,
    ) -> CompareAndSwap {
        self.compare_barrier.wait();
        self.inner
            .compare_and_swap(execution_id, expected, checkpoint)
    }
}

fn contending_turns(
    store: InMemoryCheckpointStore,
    workflow: LinearWorkflow,
    execution_id: ExecutionId,
    input: ExactBytes,
) -> [HostOutcome; 2] {
    let (first_store, second_store) = ContendedStore::pair(store);
    thread::scope(|scope| {
        let first_workflow = workflow.clone();
        let first_input = input.clone();
        let first = scope.spawn(move || {
            let mut durable_host = DurableHost::new(first_store, epoch(201));
            durable_host.turn(&first_workflow, execution_id, first_input)
        });
        let second = scope.spawn(move || {
            let mut durable_host = DurableHost::new(second_store, epoch(202));
            durable_host.turn(&workflow, execution_id, input)
        });
        [first.join().unwrap(), second.join().unwrap()]
    })
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

fn schedule(
    host: &mut DurableHost<InMemoryCheckpointStore>,
    workflow: &LinearWorkflow,
    execution_id: ExecutionId,
    input: &ExactBytes,
) -> Option<LogicalActivityId> {
    match host.turn(workflow, execution_id, input.clone()) {
        HostOutcome::ScheduleAccepted { activity, .. } => Some(activity),
        _ => None,
    }
}

fn expose(
    host: &mut DurableHost<InMemoryCheckpointStore>,
    workflow: &LinearWorkflow,
    execution_id: ExecutionId,
    input: &ExactBytes,
) -> Option<DispatchPermit> {
    match host.turn(workflow, execution_id, input.clone()) {
        HostOutcome::DispatchPermitted { permit, .. } => Some(permit),
        _ => None,
    }
}

fn prepared_one(
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
    let logical = schedule(&mut durable_host, &workflow, execution_id, &input).unwrap();
    let permit = expose(&mut durable_host, &workflow, execution_id, &input).unwrap();
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

fn restart_before_schedule(id: ScenarioId) -> ScenarioEvidence {
    let store = InMemoryCheckpointStore::new();
    let workflow = LinearWorkflow::one("effect", 1, b"A");
    let execution_id = execution(1);
    let input = bytes(b"workflow");
    let mut first = host(store.clone(), 1);
    store.fail_next_compare_and_swap(InMemoryFault::RejectBeforeApply);
    let rejected = first.turn(&workflow, execution_id, input.clone());
    let absent = store.load(execution_id).is_none();
    let mut restarted = host(store.clone(), 2);
    let accepted = restarted.turn(&workflow, execution_id, input);
    ScenarioEvidence::new(
        id,
        "reject schedule before apply, discard host, and replay",
        [
            (
                "rejected schedule requires reload and grants no permit",
                matches!(
                    rejected,
                    HostOutcome::ReloadRequired {
                        boundary: PersistenceBoundary::Schedule,
                        reason: ReloadReason::RejectedBeforeApply
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

fn restart_after_schedule(id: ScenarioId) -> ScenarioEvidence {
    let store = InMemoryCheckpointStore::new();
    let workflow = LinearWorkflow::one("effect", 1, b"A");
    let execution_id = execution(2);
    let input = bytes(b"workflow");
    let mut first = host(store.clone(), 1);
    let (logical, schedule_revision) = match first.turn(&workflow, execution_id, input.clone()) {
        HostOutcome::ScheduleAccepted { activity, revision } => (Some(activity), Some(revision)),
        _ => (None, None),
    };
    let mut restarted = host(store, 2);
    let (permit, exposure_revision) = match restarted.turn(&workflow, execution_id, input.clone()) {
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

fn restart_after_exposure(id: ScenarioId) -> ScenarioEvidence {
    let (store, _, workflow, execution_id, input, logical, permit) = prepared_one(3);
    let before = store.load(execution_id);
    let mut restarted = host(store.clone(), 9);
    let outcome = restarted.turn(&workflow, execution_id, input);
    let after = store.load(execution_id);
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

fn lost_reply_then_observation(id: ScenarioId) -> ScenarioEvidence {
    let store = InMemoryCheckpointStore::new();
    let workflow = LinearWorkflow::one("effect", 1, b"A");
    let execution_id = execution(4);
    let input = bytes(b"workflow");
    let mut durable_host = host(store.clone(), 4);
    let logical = schedule(&mut durable_host, &workflow, execution_id, &input).unwrap();
    store.fail_next_compare_and_swap(InMemoryFault::LoseResponseAfterApply);
    let lost = durable_host.turn(&workflow, execution_id, input.clone());
    let quarantined = durable_host.turn(&workflow, execution_id, input.clone());
    let observed = durable_host.observe(
        execution_id,
        &input,
        ActivityObservation::new(logical.clone(), bytes(b"result")),
    );
    let completed = durable_host.turn(&workflow, execution_id, input);
    ScenarioEvidence::new(
        id,
        "lose the exposure CAS response, reload quarantine, then inject a result",
        [
            (
                "lost reply grants no permit",
                matches!(
                    lost,
                    HostOutcome::ReloadRequired {
                        boundary: PersistenceBoundary::Exposure,
                        reason: ReloadReason::ResponseLostAfterApply
                    }
                ),
            ),
            (
                "accepted but unresolved exposure reloads as quarantine",
                matches!(
                    quarantined,
                    HostOutcome::Quarantined { activity, .. } if activity == logical
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
                    HostOutcome::WorkflowCompleted { result } if result == bytes(b"result")
                ),
            ),
        ],
    )
}

fn schedule_conflict(id: ScenarioId) -> ScenarioEvidence {
    let store = InMemoryCheckpointStore::new();
    let workflow = LinearWorkflow::one("effect", 1, b"A");
    let execution_id = execution(5);
    let input = bytes(b"workflow");
    let outcomes = contending_turns(store.clone(), workflow.clone(), execution_id, input.clone());
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
    let exposed = after_race.turn(&workflow, execution_id, input);
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

fn exposure_conflict(id: ScenarioId) -> ScenarioEvidence {
    let store = InMemoryCheckpointStore::new();
    let workflow = LinearWorkflow::one("effect", 1, b"A");
    let execution_id = execution(6);
    let input = bytes(b"workflow");
    let mut scheduling_host = host(store.clone(), 6);
    let logical = schedule(&mut scheduling_host, &workflow, execution_id, &input).unwrap();
    let outcomes = contending_turns(store.clone(), workflow.clone(), execution_id, input.clone());
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
    let duplicate = after_race.turn(&workflow, execution_id, input);
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

fn changed_order(id: ScenarioId) -> ScenarioEvidence {
    let store = InMemoryCheckpointStore::new();
    let original = LinearWorkflow::two();
    let execution_id = execution(7);
    let input = bytes(b"workflow");
    let mut durable_host = host(store, 7);
    let logical = schedule(&mut durable_host, &original, execution_id, &input).unwrap();
    expose(&mut durable_host, &original, execution_id, &input).unwrap();
    durable_host.observe(
        execution_id,
        &input,
        ActivityObservation::new(logical, bytes(b"done")),
    );
    let reordered = LinearWorkflow {
        activities: vec![
            (activity_name("second", 1), bytes(b"B")),
            (activity_name("first", 1), bytes(b"A")),
        ],
    };
    let outcome = durable_host.turn(&reordered, execution_id, input);
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

fn changed_name(id: ScenarioId) -> ScenarioEvidence {
    changed_definition(
        id,
        "replay a completed activity with a changed versioned name",
        LinearWorkflow::one("effect", 2, b"A"),
    )
}

fn changed_input(id: ScenarioId) -> ScenarioEvidence {
    changed_definition(
        id,
        "replay a completed activity with changed exact bytes",
        LinearWorkflow::one("effect", 1, b"a"),
    )
}

fn changed_definition(
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
    let logical = schedule(&mut durable_host, &original, execution_id, &input).unwrap();
    expose(&mut durable_host, &original, execution_id, &input).unwrap();
    durable_host.observe(
        execution_id,
        &input,
        ActivityObservation::new(logical, bytes(b"done")),
    );
    let outcome = durable_host.turn(&changed, execution_id, input);
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

fn refreshed_attempt(id: ScenarioId) -> ScenarioEvidence {
    let store = InMemoryCheckpointStore::new();
    let workflow = LinearWorkflow::one("effect", 1, b"A");
    let execution_id = execution(10);
    let input = bytes(b"workflow");
    let mut durable_host = host(store.clone(), 10);
    let logical = schedule(&mut durable_host, &workflow, execution_id, &input).unwrap();
    store.fail_next_compare_and_swap(InMemoryFault::RejectBeforeApply);
    let rejected = durable_host.turn(&workflow, execution_id, input.clone());
    let accepted = durable_host.turn(&workflow, execution_id, input);
    ScenarioEvidence::new(
        id,
        "discard a pre-exposure attempt after rejection and prepare a fresh attempt",
        [
            (
                "discarded attempt confers no permit",
                matches!(
                    rejected,
                    HostOutcome::ReloadRequired {
                        boundary: PersistenceBoundary::Exposure,
                        reason: ReloadReason::RejectedBeforeApply
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

fn quarantine(id: ScenarioId) -> ScenarioEvidence {
    let (store, mut durable_host, workflow, execution_id, input, logical, _) = prepared_one(11);
    let before = store.load(execution_id);
    let first = durable_host.turn(&workflow, execution_id, input.clone());
    let second = durable_host.turn(&workflow, execution_id, input);
    let after = store.load(execution_id);
    ScenarioEvidence::new(
        id,
        "poll exposed unresolved work repeatedly",
        [
            (
                "first replay is quarantined",
                matches!(&first, HostOutcome::Quarantined { activity, .. } if activity == &logical),
            ),
            (
                "duplicate replay remains quarantined without a permit",
                matches!(second, HostOutcome::Quarantined { .. }),
            ),
            ("quarantine does not mutate the checkpoint", before == after),
        ],
    )
}

fn completed_replay(id: ScenarioId) -> ScenarioEvidence {
    let (_, mut durable_host, workflow, execution_id, input, logical, _) = prepared_one(12);
    durable_host.observe(
        execution_id,
        &input,
        ActivityObservation::new(logical, bytes(b"recorded")),
    );
    let first = durable_host.turn(&workflow, execution_id, input.clone());
    let second = durable_host.turn(&workflow, execution_id, input);
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

fn unsupported_format(id: ScenarioId) -> ScenarioEvidence {
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
    let stored = store.compare_and_swap(execution_id, None, unsupported);
    let mut durable_host = host(store, 13);
    let outcome = durable_host.turn(&workflow, execution_id, input);
    ScenarioEvidence::new(
        id,
        "load a checkpoint with an unsupported envelope version",
        [
            (
                "test checkpoint was accepted by opaque storage",
                matches!(stored, CompareAndSwap::Accepted(_)),
            ),
            (
                "host rejects format before workflow or dispatch",
                matches!(outcome, HostOutcome::CheckpointRejected(_)),
            ),
        ],
    )
}

fn schedule_response_lost(id: ScenarioId) -> ScenarioEvidence {
    let store = InMemoryCheckpointStore::new();
    let workflow = LinearWorkflow::one("effect", 1, b"A");
    let execution_id = execution(14);
    let input = bytes(b"workflow");
    let mut durable_host = host(store.clone(), 14);
    store.fail_next_compare_and_swap(InMemoryFault::LoseResponseAfterApply);
    let lost = durable_host.turn(&workflow, execution_id, input.clone());
    let loaded = store.load(execution_id);
    let next = durable_host.turn(&workflow, execution_id, input);
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
                        reason: ReloadReason::ResponseLostAfterApply
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

fn exposure_response_lost(id: ScenarioId) -> ScenarioEvidence {
    let store = InMemoryCheckpointStore::new();
    let workflow = LinearWorkflow::one("effect", 1, b"A");
    let execution_id = execution(15);
    let input = bytes(b"workflow");
    let mut durable_host = host(store, 15);
    schedule(&mut durable_host, &workflow, execution_id, &input).unwrap();
    durable_host
        .store()
        .fail_next_compare_and_swap(InMemoryFault::LoseResponseAfterApply);
    let lost = durable_host.turn(&workflow, execution_id, input.clone());
    let next = durable_host.turn(&workflow, execution_id, input);
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
                        reason: ReloadReason::ResponseLostAfterApply
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

fn observation_response_lost(id: ScenarioId) -> ScenarioEvidence {
    let (store, mut durable_host, workflow, execution_id, input, logical, _) = prepared_one(16);
    store.fail_next_compare_and_swap(InMemoryFault::LoseResponseAfterApply);
    let lost = durable_host.observe(
        execution_id,
        &input,
        ActivityObservation::new(logical, bytes(b"observed")),
    );
    let next = durable_host.turn(&workflow, execution_id, input);
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
                        reason: ReloadReason::ResponseLostAfterApply
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

fn mismatched_observation(id: ScenarioId) -> ScenarioEvidence {
    let (_, mut durable_host, workflow, execution_id, input, logical, _) = prepared_one(17);
    let mismatched = LogicalActivityId::new(
        execution_id,
        ActivitySequence::new(0),
        activity_name("effect", 1),
        bytes(b"different"),
    );
    let rejected = durable_host.observe(
        execution_id,
        &input,
        ActivityObservation::new(mismatched, bytes(b"result")),
    );
    let next = durable_host.turn(&workflow, execution_id, input);
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

fn competing_observations(id: ScenarioId) -> ScenarioEvidence {
    let (store, _, workflow, execution_id, input, logical, _) = prepared_one(18);
    let (first_store, second_store) = ContendedStore::pair(store.clone());
    let logical_for_first = logical.clone();
    let input_for_first = input.clone();
    let replay_input = input.clone();
    let observations = thread::scope(|scope| {
        let first = scope.spawn(move || {
            let durable_host = DurableHost::new(first_store, epoch(203));
            let result = bytes(b"first");
            let outcome = durable_host.observe(
                execution_id,
                &input_for_first,
                ActivityObservation::new(logical_for_first, result.clone()),
            );
            (result, outcome)
        });
        let second = scope.spawn(move || {
            let durable_host = DurableHost::new(second_store, epoch(204));
            let result = bytes(b"second");
            let outcome = durable_host.observe(
                execution_id,
                &input,
                ActivityObservation::new(logical, result.clone()),
            );
            (result, outcome)
        });
        [first.join().unwrap(), second.join().unwrap()]
    });
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
    let completed = durable_host.turn(&workflow, execution_id, replay_input);
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

fn quarantine_resolution(id: ScenarioId) -> ScenarioEvidence {
    let (_, mut durable_host, workflow, execution_id, input, logical, permit) = prepared_one(19);
    let quarantined = durable_host.turn(&workflow, execution_id, input.clone());
    let accepted = durable_host.observe(
        execution_id,
        &input,
        ActivityObservation::new(logical.clone(), bytes(b"resolved")),
    );
    let completed = durable_host.turn(&workflow, execution_id, input);
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
                "resolution replays without redispatch",
                matches!(completed, HostOutcome::WorkflowCompleted { .. }),
            ),
        ],
    )
}

fn quarantine_blocks_progress(id: ScenarioId) -> ScenarioEvidence {
    let store = InMemoryCheckpointStore::new();
    let workflow = LinearWorkflow::two();
    let execution_id = execution(20);
    let input = bytes(b"workflow");
    let mut durable_host = host(store.clone(), 20);
    let first_logical = schedule(&mut durable_host, &workflow, execution_id, &input).unwrap();
    expose(&mut durable_host, &workflow, execution_id, &input).unwrap();
    let before = store.load(execution_id);
    let blocked_one = durable_host.turn(&workflow, execution_id, input.clone());
    let blocked_two = durable_host.turn(&workflow, execution_id, input.clone());
    let still_same = before == store.load(execution_id);
    durable_host.observe(
        execution_id,
        &input,
        ActivityObservation::new(first_logical, bytes(b"first-result")),
    );
    let after_observation = durable_host.turn(&workflow, execution_id, input);
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
use std::{
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{Arc, Barrier},
    thread,
};
