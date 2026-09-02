use std::collections::VecDeque;
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

use async_trait::async_trait;
use tokio::sync::{mpsc, oneshot, watch};
use tracing::{debug, info, warn};

use crate::Result;
use crate::add_replica::{
    AddReplicaCoordinatorPhase, AddReplicaProgress, AddReplicaTerminalResult,
};
use crate::error::KubericError;
use crate::pod::{
    RuntimeControlSnapshot, RuntimeEffect, RuntimeEffectCommand, RuntimeEffectResult,
};
use crate::remove_replica::{
    REMOVE_REPLICA_CALL_TIMEOUT_SECONDS, RemoveReplicaClock, RemoveReplicaCoordinatorPhase,
    RemoveReplicaIntent, RemoveReplicaMode, RemoveReplicaProgress, RemoveReplicaTerminalResult,
    SystemRemoveReplicaClock, TargetRetirementObservation, normalize_remove_error,
};
use crate::replica_lifecycle::{
    PEER_STAGE_SEMANTIC_VERSION, PEER_TERMINAL_RETENTION, PeerLifecycleStatus, PeerOperationKind,
    PeerStage, PeerStageObservation, PeerStageRequest, PeerStageState,
    REPLICA_LIFECYCLE_PEER_PROTOCOL_VERSION,
};
use crate::types::{
    AccessStatus, AgentControlVersion, AgentGeneration, CancellationToken,
    CorrelatedActionObservation, CorrelatedControlActionAcknowledgement,
    CorrelatedControlActionRequest, DurableActionErrorClass, DurableActionObservation,
    DurableActionResult, DurableActionState, DurableReplicaAction, FaultType, LocalFaultRecord,
    Lsn, ReplicaAgentStatus, ReplicaId, ReplicaInfo, ReplicaInstanceId, ReplicaSetQuorumMode,
    ReplicaStatusInfo, Role,
};

pub const TERMINAL_RETENTION: usize = 16;
pub const FAULT_RETENTION: usize = 16;
const MAX_ERROR_BYTES: usize = 1024;
pub const CORRELATED_CONTROL_PROTOCOL_VERSION: u32 = 3;

/// Transport-facing commands accepted by the pod-local replica agent.
pub enum AgentCommand {
    ExecuteCorrelatedControlAction {
        request: Box<CorrelatedControlActionRequest>,
        reply: oneshot::Sender<Result<CorrelatedControlActionAcknowledgement>>,
    },
    GetStatus {
        reply: oneshot::Sender<ReplicaStatusInfo>,
    },
    ExecuteLifecycleStage {
        request: Box<PeerStageRequest>,
        reply: oneshot::Sender<Result<PeerStageObservation>>,
    },
    GetLifecycleStatus {
        target_replica_id: ReplicaId,
        target_instance_id: ReplicaInstanceId,
        expected_generation: AgentGeneration,
        reply: oneshot::Sender<Result<PeerLifecycleStatus>>,
    },
}

struct ActiveCorrelated {
    execution_id: RuntimeEffectExecutionId,
    coordinator_execution_id: Option<CoordinatorExecutionId>,
    coordinator_authority_tx: Option<watch::Sender<Option<CoordinatorAuthority>>>,
    remove_intent: Option<RemoveReplicaIntent>,
    observation: CorrelatedActionObservation,
    reply: Option<oneshot::Sender<Result<CorrelatedControlActionAcknowledgement>>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RuntimeEffectExecutionId {
    generation: AgentGeneration,
    sequence: u64,
}

struct RuntimeCompletion {
    execution_id: RuntimeEffectExecutionId,
    result: Result<RuntimeEffectResult>,
}

struct ActivePeerStage {
    execution_id: RuntimeEffectExecutionId,
    observation: PeerStageObservation,
}

struct PeerStageCompletion {
    execution_id: RuntimeEffectExecutionId,
    state: PeerStageState,
    error: Option<String>,
}

struct PeerSenderFence {
    epoch: crate::types::Epoch,
    sender_replica_id: ReplicaId,
    sender_instance_id: ReplicaInstanceId,
    sender_generation: AgentGeneration,
    sender_control_address: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CoordinatorExecutionId {
    generation: AgentGeneration,
    action_id: String,
    attempt_id: String,
    phase_sequence: u64,
}

#[derive(Clone)]
struct CoordinatorExecution {
    generation: AgentGeneration,
    action_id: String,
    attempt_id: String,
    phase_sequence: Arc<AtomicU64>,
}

impl CoordinatorExecution {
    fn new(generation: AgentGeneration, action_id: String, attempt_id: String) -> Self {
        Self {
            generation,
            action_id,
            attempt_id,
            phase_sequence: Arc::new(AtomicU64::new(0)),
        }
    }

    fn current_id(&self) -> CoordinatorExecutionId {
        CoordinatorExecutionId {
            generation: self.generation.clone(),
            action_id: self.action_id.clone(),
            attempt_id: self.attempt_id.clone(),
            phase_sequence: self.phase_sequence.load(Ordering::SeqCst),
        }
    }

    fn advance(&self) -> CoordinatorExecutionId {
        self.phase_sequence.fetch_add(1, Ordering::SeqCst);
        self.current_id()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CoordinatorAuthority {
    generation: AgentGeneration,
    action_id: String,
    action_signature: String,
    attempt_id: String,
    phase_sequence: u64,
}

enum CoordinatorUpdate {
    AddProgress(AddReplicaProgress),
    AddTerminal(std::result::Result<AddReplicaTerminalResult, String>),
    RemoveProgress(RemoveReplicaProgress),
    RemoveTerminal(std::result::Result<RemoveReplicaTerminalResult, String>),
}

struct CoordinatorEvent {
    execution_id: CoordinatorExecutionId,
    update: CoordinatorUpdate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LifecyclePeerTransportErrorKind {
    Unavailable,
    Stale,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LifecyclePeerTransportError {
    kind: LifecyclePeerTransportErrorKind,
    message: String,
}

impl LifecyclePeerTransportError {
    fn unavailable(message: impl Into<String>) -> Self {
        Self {
            kind: LifecyclePeerTransportErrorKind::Unavailable,
            message: normalize_remove_error(&message.into()),
        }
    }

    fn stale(message: impl Into<String>) -> Self {
        Self {
            kind: LifecyclePeerTransportErrorKind::Stale,
            message: normalize_remove_error(&message.into()),
        }
    }
}

#[async_trait]
trait LifecyclePeerTransport: Send + Sync {
    async fn get_status(
        &self,
        intent: &RemoveReplicaIntent,
        timeout: std::time::Duration,
    ) -> std::result::Result<PeerLifecycleStatus, LifecyclePeerTransportError>;

    async fn execute_stage(
        &self,
        intent: &RemoveReplicaIntent,
        request: PeerStageRequest,
        timeout: std::time::Duration,
    ) -> std::result::Result<PeerStageObservation, LifecyclePeerTransportError>;
}

#[derive(Debug, Default)]
struct GrpcLifecyclePeerTransport;

#[async_trait]
impl LifecyclePeerTransport for GrpcLifecyclePeerTransport {
    async fn get_status(
        &self,
        intent: &RemoveReplicaIntent,
        timeout: std::time::Duration,
    ) -> std::result::Result<PeerLifecycleStatus, LifecyclePeerTransportError> {
        let address = intent.target_control_address.clone().ok_or_else(|| {
            LifecyclePeerTransportError::stale(
                "remove intent has no frozen target lifecycle endpoint",
            )
        })?;
        let generation = intent
            .expected_target_agent_generation
            .clone()
            .ok_or_else(|| {
                LifecyclePeerTransportError::stale(
                    "remove intent has no frozen target agent generation",
                )
            })?;
        let started = std::time::Instant::now();
        let client = crate::grpc::peer_client::GrpcPeerClient::connect_with_timeout(
            address,
            intent.target_replica_id,
            intent.target_instance_id.clone(),
            generation,
            timeout,
        )
        .await
        .map_err(lifecycle_transport_error)?;
        let remaining = timeout.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            return Err(LifecyclePeerTransportError::unavailable(
                "target lifecycle status call budget expired during connect",
            ));
        }
        client
            .get_status(remaining)
            .await
            .map_err(lifecycle_transport_error)
    }

    async fn execute_stage(
        &self,
        intent: &RemoveReplicaIntent,
        request: PeerStageRequest,
        timeout: std::time::Duration,
    ) -> std::result::Result<PeerStageObservation, LifecyclePeerTransportError> {
        let address = intent.target_control_address.clone().ok_or_else(|| {
            LifecyclePeerTransportError::stale(
                "remove intent has no frozen target lifecycle endpoint",
            )
        })?;
        let generation = intent
            .expected_target_agent_generation
            .clone()
            .ok_or_else(|| {
                LifecyclePeerTransportError::stale(
                    "remove intent has no frozen target agent generation",
                )
            })?;
        let started = std::time::Instant::now();
        let client = crate::grpc::peer_client::GrpcPeerClient::connect_with_timeout(
            address,
            intent.target_replica_id,
            intent.target_instance_id.clone(),
            generation,
            timeout,
        )
        .await
        .map_err(lifecycle_transport_error)?;
        let remaining = timeout.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            return Err(LifecyclePeerTransportError::unavailable(
                "target lifecycle stage call budget expired during connect",
            ));
        }
        client
            .execute_stage(request, remaining)
            .await
            .map_err(lifecycle_transport_error)
    }
}

fn lifecycle_transport_error(error: KubericError) -> LifecyclePeerTransportError {
    match error {
        KubericError::RemotePeerRequestRejected(message)
        | KubericError::RemoteAgentPreconditionRejected(message)
        | KubericError::RemoteControlProtocolUnsupported(message)
        | KubericError::RemoteAgentConflict(message) => LifecyclePeerTransportError::stale(message),
        other => LifecyclePeerTransportError::unavailable(other.to_string()),
    }
}

#[cfg(test)]
#[derive(Debug, Default)]
struct UnavailableLifecyclePeerTransport;

#[cfg(test)]
#[async_trait]
impl LifecyclePeerTransport for UnavailableLifecyclePeerTransport {
    async fn get_status(
        &self,
        _intent: &RemoveReplicaIntent,
        _timeout: std::time::Duration,
    ) -> std::result::Result<PeerLifecycleStatus, LifecyclePeerTransportError> {
        Err(LifecyclePeerTransportError::unavailable(
            "test lifecycle peer is unavailable",
        ))
    }

    async fn execute_stage(
        &self,
        _intent: &RemoveReplicaIntent,
        _request: PeerStageRequest,
        _timeout: std::time::Duration,
    ) -> std::result::Result<PeerStageObservation, LifecyclePeerTransportError> {
        Err(LifecyclePeerTransportError::unavailable(
            "test lifecycle peer is unavailable",
        ))
    }
}

/// Pod-local correlated-control owner between gRPC and ordered runtime effects.
pub struct ReplicaAgent {
    replica_id: ReplicaId,
    instance_id: ReplicaInstanceId,
    generation: AgentGeneration,
    control_version: AgentControlVersion,
    execution_sequence: u64,
    fault_sequence: u64,
    command_rx: mpsc::Receiver<AgentCommand>,
    runtime_tx: mpsc::Sender<RuntimeEffectCommand>,
    runtime_status_rx: watch::Receiver<RuntimeControlSnapshot>,
    fault_rx: mpsc::Receiver<FaultType>,
    fault_rx_open: bool,
    command_rx_open: bool,
    shutdown: CancellationToken,
    completion_tx: mpsc::UnboundedSender<RuntimeCompletion>,
    completion_rx: mpsc::UnboundedReceiver<RuntimeCompletion>,
    peer_completion_tx: mpsc::UnboundedSender<PeerStageCompletion>,
    peer_completion_rx: mpsc::UnboundedReceiver<PeerStageCompletion>,
    coordinator_tx: mpsc::UnboundedSender<CoordinatorEvent>,
    coordinator_rx: mpsc::UnboundedReceiver<CoordinatorEvent>,
    active: Option<ActiveCorrelated>,
    peer_control_version: u64,
    peer_sender_fence: Option<PeerSenderFence>,
    active_peer: Option<ActivePeerStage>,
    peer_terminals: VecDeque<PeerStageObservation>,
    terminals: VecDeque<CorrelatedActionObservation>,
    faults: VecDeque<LocalFaultRecord>,
    remove_clock: Arc<dyn RemoveReplicaClock>,
    lifecycle_peer_transport: Arc<dyn LifecyclePeerTransport>,
    shutting_down: bool,
}

impl ReplicaAgent {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        replica_id: ReplicaId,
        instance_id: ReplicaInstanceId,
        command_rx: mpsc::Receiver<AgentCommand>,
        runtime_tx: mpsc::Sender<RuntimeEffectCommand>,
        runtime_status_rx: watch::Receiver<RuntimeControlSnapshot>,
        fault_rx: mpsc::Receiver<FaultType>,
        shutdown: CancellationToken,
    ) -> Self {
        Self::new_with_clock(
            replica_id,
            instance_id,
            command_rx,
            runtime_tx,
            runtime_status_rx,
            fault_rx,
            shutdown,
            Arc::new(SystemRemoveReplicaClock),
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new_with_clock(
        replica_id: ReplicaId,
        instance_id: ReplicaInstanceId,
        command_rx: mpsc::Receiver<AgentCommand>,
        runtime_tx: mpsc::Sender<RuntimeEffectCommand>,
        runtime_status_rx: watch::Receiver<RuntimeControlSnapshot>,
        fault_rx: mpsc::Receiver<FaultType>,
        shutdown: CancellationToken,
        remove_clock: Arc<dyn RemoveReplicaClock>,
    ) -> Self {
        Self::new_with_clock_and_transport(
            replica_id,
            instance_id,
            command_rx,
            runtime_tx,
            runtime_status_rx,
            fault_rx,
            shutdown,
            remove_clock,
            Arc::new(GrpcLifecyclePeerTransport),
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn new_with_clock_and_transport(
        replica_id: ReplicaId,
        instance_id: ReplicaInstanceId,
        command_rx: mpsc::Receiver<AgentCommand>,
        runtime_tx: mpsc::Sender<RuntimeEffectCommand>,
        runtime_status_rx: watch::Receiver<RuntimeControlSnapshot>,
        fault_rx: mpsc::Receiver<FaultType>,
        shutdown: CancellationToken,
        remove_clock: Arc<dyn RemoveReplicaClock>,
        lifecycle_peer_transport: Arc<dyn LifecyclePeerTransport>,
    ) -> Self {
        let generation = AgentGeneration::generate();
        let (completion_tx, completion_rx) = mpsc::unbounded_channel();
        let (peer_completion_tx, peer_completion_rx) = mpsc::unbounded_channel();
        let (coordinator_tx, coordinator_rx) = mpsc::unbounded_channel();
        info!(
            replica_id,
            instance_id = %instance_id,
            agent_generation = %generation,
            "ReplicaAgent process generation created"
        );
        Self {
            replica_id,
            instance_id,
            generation,
            control_version: AgentControlVersion::default(),
            execution_sequence: 0,
            fault_sequence: 0,
            command_rx,
            runtime_tx,
            runtime_status_rx,
            fault_rx,
            fault_rx_open: true,
            command_rx_open: true,
            shutdown,
            completion_tx,
            completion_rx,
            peer_completion_tx,
            peer_completion_rx,
            coordinator_tx,
            coordinator_rx,
            active: None,
            peer_control_version: 0,
            peer_sender_fence: None,
            active_peer: None,
            peer_terminals: VecDeque::with_capacity(PEER_TERMINAL_RETENTION),
            terminals: VecDeque::with_capacity(TERMINAL_RETENTION),
            faults: VecDeque::with_capacity(FAULT_RETENTION),
            remove_clock,
            lifecycle_peer_transport,
            shutting_down: false,
        }
    }

    pub async fn serve(mut self) {
        info!(
            replica_id = self.replica_id,
            agent_generation = %self.generation,
            "ReplicaAgent serve loop started"
        );
        loop {
            if self.shutting_down && self.active.is_none() && self.active_peer.is_none() {
                break;
            }
            tokio::select! {
                biased;
                completion = self.completion_rx.recv() => {
                    if let Some(completion) = completion {
                        self.handle_completion(completion);
                    }
                }
                completion = self.peer_completion_rx.recv() => {
                    if let Some(completion) = completion {
                        self.handle_peer_completion(completion);
                    }
                }
                event = self.coordinator_rx.recv() => {
                    if let Some(event) = event {
                        self.handle_coordinator_event(event);
                    }
                }
                fault = self.fault_rx.recv(), if self.fault_rx_open => {
                    match fault {
                        Some(fault) => self.record_fault(fault),
                        None => self.fault_rx_open = false,
                    }
                }
                command = self.command_rx.recv(), if self.command_rx_open => {
                    match command {
                        Some(AgentCommand::GetStatus { reply }) => {
                            let _ = reply.send(self.status());
                        }
                        Some(AgentCommand::GetLifecycleStatus {
                            target_replica_id,
                            target_instance_id,
                            expected_generation,
                            reply,
                        }) => {
                            let _ = reply.send(self.peer_status(
                                target_replica_id,
                                target_instance_id,
                                expected_generation,
                            ));
                        }
                        Some(command) if self.shutting_down => {
                            reject_agent_command(command, KubericError::Closed);
                        }
                        Some(AgentCommand::ExecuteCorrelatedControlAction { request, reply }) => {
                            self.accept_correlated(*request, reply);
                        }
                        Some(AgentCommand::ExecuteLifecycleStage { request, reply }) => {
                            self.accept_peer_stage(*request, reply);
                        }
                        None => {
                            self.command_rx_open = false;
                            self.begin_shutdown();
                        }
                    }
                }
                _ = self.shutdown.cancelled(), if !self.shutting_down => {
                    self.begin_shutdown();
                }
            }
        }
        info!(
            replica_id = self.replica_id,
            agent_generation = %self.generation,
            "ReplicaAgent serve loop stopped"
        );
    }

    fn accept_correlated(
        &mut self,
        request: CorrelatedControlActionRequest,
        reply: oneshot::Sender<Result<CorrelatedControlActionAcknowledgement>>,
    ) {
        if matches!(
            &request.action,
            DurableReplicaAction::RemoveReplicaIntent { .. }
        ) {
            self.accept_remove_replica_intent(request, reply);
            return;
        }
        if request.protocol_version != CORRELATED_CONTROL_PROTOCOL_VERSION {
            let _ = reply.send(Err(KubericError::UnsupportedControlProtocolVersion {
                got: request.protocol_version,
            }));
            return;
        }
        if request.action_id.is_empty() {
            let _ = reply.send(Err(KubericError::InvalidCorrelatedActionId));
            return;
        }
        if request.target_replica_id != self.replica_id
            || request.target_instance_id != self.instance_id
        {
            let _ = reply.send(Err(KubericError::CorrelatedTargetMismatch {
                expected_id: self.replica_id,
                expected_instance: self.instance_id.clone(),
                actual_id: request.target_replica_id,
                actual_instance: request.target_instance_id,
            }));
            return;
        }
        if request.expected_agent_generation != self.generation {
            let _ = reply.send(Err(KubericError::StaleAgentGeneration {
                expected: request.expected_agent_generation,
                current: self.generation.clone(),
            }));
            return;
        }
        let actual_signature = request.action.signature();
        if request.input_signature != actual_signature {
            let _ = reply.send(Err(KubericError::ActionSignatureMismatch {
                action_id: request.action_id,
            }));
            return;
        }
        if let Some(observed) = self.find_correlated(&request.action_id) {
            if observed.action.signature != request.input_signature {
                warn!(
                    action_id = request.action_id,
                    agent_generation = %self.generation,
                    "rejecting correlated action ID reused with different input"
                );
                let _ = reply.send(Err(KubericError::ActionIdConflict {
                    action_id: request.action_id,
                }));
            } else {
                debug!(
                    action_id = request.action_id,
                    state = ?observed.action.state,
                    agent_generation = %self.generation,
                    "replaying correlated action observation"
                );
                send_observation(reply, observed.clone());
            }
            return;
        }
        if request.expected_control_version != self.control_version {
            let error = if request.expected_control_version < self.control_version {
                KubericError::CorrelatedContinuityUnavailable {
                    action_id: request.action_id,
                }
            } else {
                KubericError::StaleAgentControlVersion {
                    expected: request.expected_control_version,
                    current: self.control_version,
                }
            };
            let _ = reply.send(Err(error));
            return;
        }
        if self.active.is_some() || self.active_peer.is_some() {
            let _ = reply.send(Err(KubericError::AgentBusy));
            return;
        }
        let runtime_epoch = self.runtime_status_rx.borrow().epoch;
        if request.observed_runtime_epoch != runtime_epoch {
            let _ = reply.send(Err(KubericError::StaleEpoch {
                got: request.observed_runtime_epoch,
                current: runtime_epoch,
            }));
            return;
        }
        if self.shutting_down {
            let _ = reply.send(Err(KubericError::Closed));
            return;
        }

        let control_version = self.control_version.advance();
        let mut observation = CorrelatedActionObservation {
            generation: self.generation.clone(),
            control_version,
            action: DurableActionObservation {
                action_id: request.action_id.clone(),
                signature: request.input_signature,
                state: DurableActionState::Scheduled,
                error_class: None,
                error: None,
                result: None,
                add_replica_progress: None,
                remove_replica_progress: None,
            },
        };
        debug!(
            action_id = request.action_id,
            control_version = control_version.value(),
            agent_generation = %self.generation,
            "accepted correlated action"
        );

        if let DurableReplicaAction::AddReplicaIntent { intent } = &request.action {
            if let Err(error) = intent.validate() {
                self.fail_before_dispatch(observation, reply, error);
                return;
            }
            let runtime = self.runtime_status_rx.borrow();
            if intent.primary_replica_id != self.replica_id
                || intent.primary_instance_id != self.instance_id
                || intent.primary_agent_generation != self.generation
                || runtime.role != crate::types::Role::Primary
                || runtime.epoch != intent.epoch
                || runtime.partition_state.is_none()
            {
                drop(runtime);
                self.fail_before_dispatch(
                    observation,
                    reply,
                    "add-replica intent does not match the active primary runtime",
                );
                return;
            }
            drop(runtime);
            observation.action.state = DurableActionState::InProgress;
            observation.action.add_replica_progress = Some(AddReplicaProgress {
                phase: AddReplicaCoordinatorPhase::Validating,
                commit_observed: false,
                copy_lsn: None,
            });
            send_observation(reply, observation.clone());
            let execution_id = self.next_execution_id();
            let coordinator_execution = CoordinatorExecution::new(
                self.generation.clone(),
                request.action_id.clone(),
                intent.attempt_id.clone(),
            );
            let parent_action_signature = observation.action.signature.clone();
            self.active = Some(ActiveCorrelated {
                execution_id,
                coordinator_execution_id: Some(coordinator_execution.current_id()),
                coordinator_authority_tx: None,
                remove_intent: None,
                observation,
                reply: None,
            });
            let coordinator_tx = self.coordinator_tx.clone();
            let runtime_tx = self.runtime_tx.clone();
            let runtime_status_rx = self.runtime_status_rx.clone();
            let intent = (**intent).clone();
            let parent_action_id = request.action_id;
            let shutdown = self.shutdown.child_token();
            let cancelled_tx = coordinator_tx.clone();
            let cancelled_execution = coordinator_execution.clone();
            tokio::spawn(async move {
                tokio::select! {
                    _ = shutdown.cancelled() => {
                        let _ = cancelled_tx.send(CoordinatorEvent {
                            execution_id: cancelled_execution.current_id(),
                            update: CoordinatorUpdate::AddTerminal(Err(
                                "add-replica coordinator cancelled by agent shutdown".to_string(),
                            )),
                        });
                    }
                    _ = run_add_replica_coordinator(
                        coordinator_execution,
                        intent,
                        parent_action_id,
                        parent_action_signature,
                        runtime_tx,
                        runtime_status_rx,
                        coordinator_tx,
                    ) => {}
                }
            });
            return;
        }

        if let DurableReplicaAction::OnDataLoss { epoch } = request.action {
            let runtime = self.runtime_status_rx.borrow();
            if runtime.epoch != epoch {
                let error = format!(
                    "data-loss action epoch {:?} does not match runtime epoch {:?}",
                    epoch, runtime.epoch
                );
                drop(runtime);
                observation.action.state = DurableActionState::Failed;
                observation.action.error_class = Some(DurableActionErrorClass::StaleEpoch);
                observation.action.error = Some(normalize_error(&error));
                send_observation(reply, observation.clone());
                self.retain_terminal(observation);
                warn!(
                    action_id = request.action_id,
                    expected_epoch = ?epoch,
                    runtime_epoch = ?runtime_epoch,
                    "rejecting durable data-loss action at mismatched epoch"
                );
                return;
            }
            if runtime.partition_state.is_none() {
                drop(runtime);
                self.fail_before_dispatch(observation, reply, "replicator not opened");
                return;
            }
            drop(runtime);
            observation.action.state = DurableActionState::InProgress;
            send_observation(reply, observation.clone());
            self.start_effect(
                observation,
                None,
                RuntimeEffect::OnDataLoss {
                    expected_epoch: epoch,
                },
            );
            return;
        }

        if let DurableReplicaAction::BuildReplica { replica } = request.action {
            if self.runtime_status_rx.borrow().partition_state.is_none() {
                self.fail_before_dispatch(observation, reply, "replicator not opened");
                return;
            }
            observation.action.state = DurableActionState::InProgress;
            send_observation(reply, observation.clone());
            self.start_effect(observation, None, RuntimeEffect::BuildReplica { replica });
            return;
        }

        let effect = correlated_runtime_effect(request.action);
        observation.action.state = DurableActionState::InProgress;
        self.start_effect(observation, Some(reply), effect);
    }

    fn accept_remove_replica_intent(
        &mut self,
        request: CorrelatedControlActionRequest,
        reply: oneshot::Sender<Result<CorrelatedControlActionAcknowledgement>>,
    ) {
        if request.protocol_version != CORRELATED_CONTROL_PROTOCOL_VERSION {
            let _ = reply.send(Err(KubericError::UnsupportedControlProtocolVersion {
                got: request.protocol_version,
            }));
            return;
        }
        let DurableReplicaAction::RemoveReplicaIntent { intent } = &request.action else {
            let _ = reply.send(Err(KubericError::RemoteAgentRequestRejected(
                "remove admission accepts only RemoveReplicaIntent".to_string(),
            )));
            return;
        };
        if request.action_id.is_empty() || request.action_id != intent.action_id {
            let _ = reply.send(Err(KubericError::InvalidCorrelatedActionId));
            return;
        }
        if let Err(error) = intent.validate() {
            let _ = reply.send(Err(KubericError::RemoteAgentRequestRejected(
                normalize_remove_error(&error),
            )));
            return;
        }
        if request.target_replica_id != self.replica_id
            || request.target_instance_id != self.instance_id
            || intent.primary_replica_id != self.replica_id
            || intent.primary_instance_id != self.instance_id
        {
            let _ = reply.send(Err(KubericError::CorrelatedTargetMismatch {
                expected_id: self.replica_id,
                expected_instance: self.instance_id.clone(),
                actual_id: request.target_replica_id,
                actual_instance: request.target_instance_id,
            }));
            return;
        }
        if request.expected_agent_generation != self.generation
            || intent.primary_agent_generation != self.generation
        {
            let _ = reply.send(Err(KubericError::StaleAgentGeneration {
                expected: request.expected_agent_generation,
                current: self.generation.clone(),
            }));
            return;
        }
        let actual_signature = request.action.signature();
        if request.input_signature != actual_signature
            || request.input_signature != intent.input_signature
        {
            let _ = reply.send(Err(KubericError::ActionSignatureMismatch {
                action_id: request.action_id,
            }));
            return;
        }
        if let Some(observed) = self.find_correlated(&request.action_id) {
            if observed.action.signature != request.input_signature {
                let _ = reply.send(Err(KubericError::ActionIdConflict {
                    action_id: request.action_id,
                }));
            } else {
                debug!(
                    action_id = request.action_id,
                    attempt_id = intent.attempt_id,
                    state = ?observed.action.state,
                    "replaying remove-replica coordinator observation"
                );
                send_observation(reply, observed.clone());
            }
            return;
        }
        if request.expected_control_version != self.control_version
            || intent.primary_agent_control_version != self.control_version
        {
            let error = if request.expected_control_version < self.control_version {
                KubericError::CorrelatedContinuityUnavailable {
                    action_id: request.action_id,
                }
            } else {
                KubericError::StaleAgentControlVersion {
                    expected: request.expected_control_version,
                    current: self.control_version,
                }
            };
            let _ = reply.send(Err(error));
            return;
        }
        if self.active.is_some() || self.active_peer.is_some() {
            let _ = reply.send(Err(KubericError::AgentBusy));
            return;
        }
        if self.shutting_down {
            let _ = reply.send(Err(KubericError::Closed));
            return;
        }

        let runtime = self.runtime_status_rx.borrow().clone();
        let allowed_configuration = runtime.configuration.as_ref().is_some_and(|configuration| {
            configuration == &intent.previous_status()
                || configuration == &intent.reduced_catch_up_status()
                || configuration == &intent.reduced_current_status()
        });
        if request.observed_runtime_epoch != intent.epoch
            || runtime.instance_id != self.instance_id
            || runtime.role != Role::Primary
            || runtime.epoch != intent.epoch
            || runtime.partition_state.is_none()
            || runtime
                .partition_state
                .as_deref()
                .is_none_or(|state| state.write_status() != AccessStatus::Granted)
            || !allowed_configuration
        {
            let control_version = self.control_version.advance();
            let observation = CorrelatedActionObservation {
                generation: self.generation.clone(),
                control_version,
                action: DurableActionObservation {
                    action_id: request.action_id,
                    signature: request.input_signature,
                    state: DurableActionState::Failed,
                    error_class: Some(
                        if runtime.partition_state.is_none() || runtime.role == Role::Unknown {
                            DurableActionErrorClass::Closed
                        } else if runtime.role != Role::Primary {
                            DurableActionErrorClass::NotPrimary
                        } else if runtime.epoch != intent.epoch {
                            DurableActionErrorClass::StaleEpoch
                        } else {
                            DurableActionErrorClass::Internal
                        },
                    ),
                    error: Some(normalize_remove_error(
                        "remove-replica primary runtime is not the exact live Primary at the frozen epoch and configuration",
                    )),
                    result: None,
                    add_replica_progress: None,
                    remove_replica_progress: None,
                },
            };
            send_observation(reply, observation.clone());
            self.retain_terminal(observation);
            return;
        }
        if self.remove_clock.unix_seconds() >= intent.overall_deadline_unix_seconds {
            let _ = reply.send(Err(KubericError::RemoteAgentPreconditionRejected(
                "remove-replica intent overall deadline has expired".to_string(),
            )));
            return;
        }

        let control_version = self.control_version.advance();
        let progress = RemoveReplicaProgress {
            phase: RemoveReplicaCoordinatorPhase::Validating,
            attempt_id: intent.attempt_id.clone(),
            commit_observed: false,
            commit_observed_unix_seconds: None,
            connection_absent: false,
            target_retirement: TargetRetirementObservation::NotAttempted,
            retirement_expiry_unix_seconds: None,
            compensation_expiry_unix_seconds: None,
            error: None,
            current_install_dispatched: false,
        };
        if let Err(error) = intent.validate_progress(&progress) {
            let _ = reply.send(Err(KubericError::RemoteAgentRequestRejected(error)));
            return;
        }
        let observation = CorrelatedActionObservation {
            generation: self.generation.clone(),
            control_version,
            action: DurableActionObservation {
                action_id: request.action_id.clone(),
                signature: request.input_signature.clone(),
                state: DurableActionState::InProgress,
                error_class: None,
                error: None,
                result: None,
                add_replica_progress: None,
                remove_replica_progress: Some(progress),
            },
        };
        let execution_id = self.next_execution_id();
        let coordinator_execution = CoordinatorExecution::new(
            self.generation.clone(),
            request.action_id.clone(),
            intent.attempt_id.clone(),
        );
        let initial_authority = CoordinatorAuthority {
            generation: self.generation.clone(),
            action_id: request.action_id.clone(),
            action_signature: request.input_signature.clone(),
            attempt_id: intent.attempt_id.clone(),
            phase_sequence: 0,
        };
        let (authority_tx, authority_rx) = watch::channel(Some(initial_authority));
        self.active = Some(ActiveCorrelated {
            execution_id,
            coordinator_execution_id: Some(coordinator_execution.current_id()),
            coordinator_authority_tx: Some(authority_tx),
            remove_intent: Some((**intent).clone()),
            observation: observation.clone(),
            reply: None,
        });
        info!(
            action_id = request.action_id,
            attempt_id = intent.attempt_id,
            agent_generation = %self.generation,
            "accepted remove-replica coordinator intent"
        );
        send_observation(reply, observation);

        let coordinator_tx = self.coordinator_tx.clone();
        let runtime_tx = self.runtime_tx.clone();
        let runtime_status_rx = self.runtime_status_rx.clone();
        let intent = (**intent).clone();
        let shutdown = self.shutdown.child_token();
        let lifecycle_peer_transport = self.lifecycle_peer_transport.clone();
        let remove_clock = self.remove_clock.clone();
        let cancelled_execution = coordinator_execution.clone();
        let cancelled_tx = coordinator_tx.clone();
        tokio::spawn(async move {
            tokio::select! {
                _ = shutdown.cancelled() => {
                    let _ = cancelled_tx.send(CoordinatorEvent {
                        execution_id: cancelled_execution.current_id(),
                        update: CoordinatorUpdate::RemoveTerminal(Err(
                            "remove-replica coordinator cancelled by agent shutdown".to_string(),
                        )),
                    });
                }
                _ = run_remove_replica_coordinator(
                    coordinator_execution,
                    intent,
                    runtime_tx,
                    runtime_status_rx,
                    authority_rx,
                    lifecycle_peer_transport,
                    remove_clock,
                    coordinator_tx,
                ) => {}
            }
        });
    }

    fn accept_peer_stage(
        &mut self,
        request: PeerStageRequest,
        reply: oneshot::Sender<Result<PeerStageObservation>>,
    ) {
        let expected_protocol_version = match request.operation_kind {
            PeerOperationKind::AddBuild | PeerOperationKind::Remove => {
                REPLICA_LIFECYCLE_PEER_PROTOCOL_VERSION
            }
        };
        if request.protocol_version != expected_protocol_version {
            let _ = reply.send(Err(KubericError::UnsupportedPeerProtocolVersion {
                got: request.protocol_version,
            }));
            return;
        }
        if let Err(error) = request.validate() {
            self.reply_peer_once(&request, PeerStageState::Rejected, error, reply);
            return;
        }
        if request.target_replica_id != self.replica_id
            || request.target_instance_id != self.instance_id
            || self.runtime_status_rx.borrow().instance_id != self.instance_id
        {
            self.reply_peer_once(
                &request,
                PeerStageState::Stale,
                format!(
                    "expected target {}@{}, got {}@{}",
                    self.replica_id,
                    self.instance_id,
                    request.target_replica_id,
                    request.target_instance_id
                ),
                reply,
            );
            return;
        }
        if request.expected_target_agent_generation != self.generation {
            self.reply_peer_once(
                &request,
                PeerStageState::Stale,
                format!(
                    "expected target generation {}, current {}",
                    request.expected_target_agent_generation, self.generation
                ),
                reply,
            );
            return;
        }
        if let Some(observation) = self.find_peer_stage(&request.message_id) {
            if observation.input_signature != request.input_signature {
                self.reply_peer_once(
                    &request,
                    PeerStageState::Conflict,
                    "peer stage message ID was reused with a different signature",
                    reply,
                );
            } else if observation.stage == PeerStage::Retire
                && observation.state == PeerStageState::Completed
                && !retire_postcondition_holds(&self.runtime_status_rx.borrow(), &request)
            {
                self.reply_peer_once(
                    &request,
                    PeerStageState::Stale,
                    "completed Retire postcondition is no longer observable",
                    reply,
                );
            } else {
                debug!(
                    message_id = request.message_id,
                    stage = ?request.stage,
                    state = ?observation.state,
                    target_agent_generation = %self.generation,
                    "replaying lifecycle peer stage observation"
                );
                let _ = reply.send(Ok(observation.clone()));
            }
            return;
        }
        if request.expected_target_peer_control_version != self.peer_control_version {
            self.reply_peer_once(
                &request,
                PeerStageState::Stale,
                format!(
                    "expected peer control version {}, current {}",
                    request.expected_target_peer_control_version, self.peer_control_version
                ),
                reply,
            );
            return;
        }
        if let Some(fence) = &self.peer_sender_fence {
            if request.epoch < fence.epoch
                || (request.epoch == fence.epoch
                    && (request.sender_replica_id != fence.sender_replica_id
                        || request.sender_instance_id != fence.sender_instance_id
                        || request.sender_agent_generation != fence.sender_generation
                        || request.sender_control_address != fence.sender_control_address))
            {
                self.reply_peer_once(
                    &request,
                    PeerStageState::Stale,
                    "peer sender conflicts with the pinned primary fence",
                    reply,
                );
                return;
            }
        }
        if self.active.is_some() || self.active_peer.is_some() {
            self.reply_peer_once(
                &request,
                PeerStageState::Rejected,
                "replica agent is busy",
                reply,
            );
            return;
        }
        if request.epoch < self.runtime_status_rx.borrow().epoch {
            self.reply_peer_once(
                &request,
                PeerStageState::Stale,
                "peer stage epoch is older than target runtime epoch",
                reply,
            );
            return;
        }
        if request.stage == PeerStage::Retire {
            let now = self.remove_clock.unix_seconds();
            if request
                .commit_observed_unix_seconds
                .is_none_or(|commit| commit > now)
            {
                self.reply_peer_once(
                    &request,
                    PeerStageState::Stale,
                    "Retire commit observation is in the future",
                    reply,
                );
                return;
            }
            if request
                .retirement_expiry_unix_seconds
                .is_none_or(|expiry| now >= expiry)
            {
                self.reply_peer_once(
                    &request,
                    PeerStageState::Stale,
                    "Retire stage deadline has expired",
                    reply,
                );
                return;
            }
        }

        self.peer_control_version = self.peer_control_version.saturating_add(1);
        self.peer_sender_fence = Some(PeerSenderFence {
            epoch: request.epoch,
            sender_replica_id: request.sender_replica_id,
            sender_instance_id: request.sender_instance_id.clone(),
            sender_generation: request.sender_agent_generation.clone(),
            sender_control_address: request.sender_control_address.clone(),
        });
        let execution_id = self.next_execution_id();
        let observation = PeerStageObservation {
            protocol_version: request.protocol_version,
            operation_kind: request.operation_kind,
            stage_semantic_version: request.stage_semantic_version,
            message_id: request.message_id.clone(),
            input_signature: request.input_signature.clone(),
            stage: request.stage,
            state: PeerStageState::Accepted,
            target_agent_generation: self.generation.clone(),
            target_peer_control_version: self.peer_control_version,
            error: None,
        };
        info!(
            message_id = request.message_id,
            operation_kind = ?request.operation_kind,
            stage = ?request.stage,
            peer_control_version = self.peer_control_version,
            target_agent_generation = %self.generation,
            "accepted lifecycle peer stage"
        );
        self.active_peer = Some(ActivePeerStage {
            execution_id: execution_id.clone(),
            observation: observation.clone(),
        });
        let _ = reply.send(Ok(observation));
        if let Some(active) = self.active_peer.as_mut() {
            active.observation.state = PeerStageState::InProgress;
        }

        let completion_tx = self.peer_completion_tx.clone();
        let runtime_tx = self.runtime_tx.clone();
        let runtime_status_rx = self.runtime_status_rx.clone();
        let remove_clock = self.remove_clock.clone();
        let shutdown = self.shutdown.child_token();
        tokio::spawn(async move {
            let result = tokio::select! {
                _ = shutdown.cancelled() => {
                    Err(PeerStageRunError::Failed(
                        "peer stage cancelled by agent shutdown".to_string(),
                    ))
                }
                result = run_peer_stage(request, runtime_tx, runtime_status_rx, remove_clock) => result,
            };
            let (state, error) = match result {
                Ok(()) => (PeerStageState::Completed, None),
                Err(PeerStageRunError::Stale(error)) => {
                    (PeerStageState::Stale, Some(normalize_error(&error)))
                }
                Err(PeerStageRunError::Failed(error)) => {
                    (PeerStageState::Failed, Some(normalize_error(&error)))
                }
            };
            let _ = completion_tx.send(PeerStageCompletion {
                execution_id,
                state,
                error,
            });
        });
    }

    fn reply_peer_once(
        &self,
        request: &PeerStageRequest,
        state: PeerStageState,
        error: impl Into<String>,
        reply: oneshot::Sender<Result<PeerStageObservation>>,
    ) {
        let error = normalize_error(&error.into());
        warn!(
            message_id = request.message_id,
            operation_kind = ?request.operation_kind,
            stage = ?request.stage,
            state = ?state,
            target_agent_generation = %self.generation,
            error,
            "lifecycle peer stage rejected before acceptance"
        );
        let _ = reply.send(Ok(PeerStageObservation {
            protocol_version: request.protocol_version,
            operation_kind: request.operation_kind,
            stage_semantic_version: request.stage_semantic_version,
            message_id: request.message_id.clone(),
            input_signature: request.input_signature.clone(),
            stage: request.stage,
            state,
            target_agent_generation: self.generation.clone(),
            target_peer_control_version: self.peer_control_version,
            error: Some(error),
        }));
    }

    fn peer_status(
        &self,
        target_replica_id: ReplicaId,
        target_instance_id: ReplicaInstanceId,
        expected_generation: AgentGeneration,
    ) -> Result<PeerLifecycleStatus> {
        if target_replica_id != self.replica_id || target_instance_id != self.instance_id {
            return Err(KubericError::PeerStageTargetMismatch(format!(
                "expected {}@{}, got {}@{}",
                self.replica_id, self.instance_id, target_replica_id, target_instance_id
            )));
        }
        if expected_generation != self.generation {
            return Err(KubericError::PeerStageStale(format!(
                "expected target generation {expected_generation}, current {}",
                self.generation
            )));
        }
        let runtime = self.runtime_status_rx.borrow();
        Ok(PeerLifecycleStatus {
            protocol_version: REPLICA_LIFECYCLE_PEER_PROTOCOL_VERSION,
            target_replica_id: self.replica_id,
            target_instance_id: self.instance_id.clone(),
            agent_generation: self.generation.clone(),
            peer_control_version: self.peer_control_version,
            role: runtime.role,
            epoch: runtime.epoch,
            healthy: runtime.partition_state.is_some(),
            current_progress: runtime
                .partition_state
                .as_deref()
                .map_or(0, |state| state.current_progress()),
            current_action: self
                .active_peer
                .as_ref()
                .map(|active| active.observation.clone()),
            retained_terminal_actions: self.peer_terminals.iter().cloned().collect(),
        })
    }

    fn handle_peer_completion(&mut self, completion: PeerStageCompletion) {
        let Some(mut active) = self.active_peer.take() else {
            warn!("discarding peer completion without active stage");
            return;
        };
        if active.execution_id != completion.execution_id {
            warn!("discarding stale peer-stage completion");
            self.active_peer = Some(active);
            return;
        }
        active.observation.state = completion.state;
        active.observation.error = completion.error;
        match active.observation.state {
            PeerStageState::Completed => info!(
                message_id = active.observation.message_id,
                operation_kind = ?active.observation.operation_kind,
                stage = ?active.observation.stage,
                target_agent_generation = %self.generation,
                "lifecycle peer stage completed"
            ),
            PeerStageState::Failed | PeerStageState::Stale => warn!(
                message_id = active.observation.message_id,
                operation_kind = ?active.observation.operation_kind,
                stage = ?active.observation.stage,
                state = ?active.observation.state,
                target_agent_generation = %self.generation,
                error = active.observation.error.as_deref().unwrap_or("unknown"),
                "lifecycle peer stage reached a terminal error"
            ),
            PeerStageState::Accepted
            | PeerStageState::InProgress
            | PeerStageState::Rejected
            | PeerStageState::Conflict => {
                warn!(
                    message_id = active.observation.message_id,
                    state = ?active.observation.state,
                    "discarding invalid lifecycle peer completion state"
                );
                return;
            }
        }
        if self.peer_terminals.len() == PEER_TERMINAL_RETENTION {
            self.peer_terminals.pop_front();
        }
        self.peer_terminals.push_back(active.observation);
    }

    fn handle_coordinator_event(&mut self, event: CoordinatorEvent) {
        let Some(active_execution) = self
            .active
            .as_ref()
            .and_then(|active| active.coordinator_execution_id.as_ref())
        else {
            warn!("discarding coordinator event without active coordinator");
            return;
        };
        let is_progress = matches!(
            &event.update,
            CoordinatorUpdate::AddProgress(_) | CoordinatorUpdate::RemoveProgress(_)
        );
        let expected_sequence = if is_progress {
            active_execution.phase_sequence.saturating_add(1)
        } else {
            active_execution.phase_sequence
        };
        if active_execution.generation != event.execution_id.generation
            || active_execution.action_id != event.execution_id.action_id
            || active_execution.attempt_id != event.execution_id.attempt_id
            || event.execution_id.phase_sequence != expected_sequence
        {
            warn!(
                action_id = event.execution_id.action_id,
                attempt_id = event.execution_id.attempt_id,
                phase_sequence = event.execution_id.phase_sequence,
                "discarding stale coordinator event"
            );
            return;
        }
        match event.update {
            CoordinatorUpdate::AddProgress(progress) => {
                let active = self.active.as_mut().unwrap();
                active.coordinator_execution_id = Some(event.execution_id);
                active.observation.action.add_replica_progress = Some(progress);
            }
            CoordinatorUpdate::AddTerminal(result) => {
                let mut active = self.active.take().unwrap();
                match result {
                    Ok(result) => {
                        active.observation.action.state = DurableActionState::Completed;
                        active.observation.action.result =
                            Some(DurableActionResult::AddReplica(result));
                    }
                    Err(error) => {
                        active.observation.action.state = DurableActionState::Failed;
                        active.observation.action.error_class =
                            Some(DurableActionErrorClass::Internal);
                        active.observation.action.error = Some(normalize_error(&error));
                    }
                }
                self.retain_terminal(active.observation);
            }
            CoordinatorUpdate::RemoveProgress(progress) => {
                let validation = self
                    .active
                    .as_ref()
                    .and_then(|active| active.remove_intent.as_ref())
                    .ok_or_else(|| "active coordinator has no remove intent".to_string())
                    .and_then(|intent| intent.validate_progress(&progress));
                if let Err(error) = validation {
                    self.fail_active_remove_coordinator(format!(
                        "invalid remove-replica coordinator progress: {error}"
                    ));
                    return;
                }
                let active = self.active.as_mut().unwrap();
                active.coordinator_execution_id = Some(event.execution_id.clone());
                active.observation.action.remove_replica_progress = Some(progress);
                if let Some(authority_tx) = &active.coordinator_authority_tx {
                    let _ = authority_tx.send(Some(CoordinatorAuthority {
                        generation: event.execution_id.generation,
                        action_id: event.execution_id.action_id,
                        action_signature: active.observation.action.signature.clone(),
                        attempt_id: event.execution_id.attempt_id,
                        phase_sequence: event.execution_id.phase_sequence,
                    }));
                }
            }
            CoordinatorUpdate::RemoveTerminal(result) => {
                let validation = match &result {
                    Ok(result) => self
                        .active
                        .as_ref()
                        .and_then(|active| {
                            Some((
                                active.remove_intent.as_ref()?,
                                active.observation.action.remove_replica_progress.as_ref()?,
                            ))
                        })
                        .ok_or_else(|| {
                            "active coordinator has no remove terminal progress".to_string()
                        })
                        .and_then(|(intent, progress)| {
                            intent.validate_terminal_progress(progress, *result)
                        }),
                    Err(_) => Ok(()),
                };
                if let Err(error) = validation {
                    self.fail_active_remove_coordinator(format!(
                        "invalid remove-replica coordinator terminal: {error}"
                    ));
                    return;
                }
                let mut active = self.active.take().unwrap();
                match result {
                    Ok(result) => {
                        let action_id =
                            normalize_remove_error(&active.observation.action.action_id);
                        let attempt_id = normalize_remove_error(&event.execution_id.attempt_id);
                        info!(
                            action_id = %action_id,
                            attempt_id = %attempt_id,
                            result = ?result,
                            "remove-replica coordinator completed"
                        );
                        active.observation.action.state = DurableActionState::Completed;
                        active.observation.action.result =
                            Some(DurableActionResult::RemoveReplica(result));
                    }
                    Err(error) => {
                        let action_id =
                            normalize_remove_error(&active.observation.action.action_id);
                        let attempt_id = normalize_remove_error(&event.execution_id.attempt_id);
                        let error = normalize_remove_error(&error);
                        warn!(
                            action_id = %action_id,
                            attempt_id = %attempt_id,
                            error = %error,
                            "remove-replica coordinator failed"
                        );
                        active.observation.action.state = DurableActionState::Failed;
                        active.observation.action.error_class =
                            Some(DurableActionErrorClass::Internal);
                        active.observation.action.error = Some(error);
                    }
                }
                self.retain_terminal(active.observation);
            }
        }
    }

    fn find_peer_stage(&self, message_id: &str) -> Option<&PeerStageObservation> {
        self.active_peer
            .as_ref()
            .filter(|active| active.observation.message_id == message_id)
            .map(|active| &active.observation)
            .or_else(|| {
                self.peer_terminals
                    .iter()
                    .rev()
                    .find(|observation| observation.message_id == message_id)
            })
    }

    fn start_effect(
        &mut self,
        observation: CorrelatedActionObservation,
        reply: Option<oneshot::Sender<Result<CorrelatedControlActionAcknowledgement>>>,
        effect: RuntimeEffect,
    ) {
        let execution_id = self.next_execution_id();
        self.dispatch_runtime(execution_id.clone(), effect);
        self.active = Some(ActiveCorrelated {
            execution_id,
            coordinator_execution_id: None,
            coordinator_authority_tx: None,
            remove_intent: None,
            observation,
            reply,
        });
    }

    fn fail_active_remove_coordinator(&mut self, error: String) {
        let Some(mut active) = self.active.take() else {
            return;
        };
        if let Some(authority_tx) = &active.coordinator_authority_tx {
            let _ = authority_tx.send(None);
        }
        let action_id = normalize_remove_error(&active.observation.action.action_id);
        let attempt_id = active
            .remove_intent
            .as_ref()
            .map_or_else(String::new, |intent| {
                normalize_remove_error(&intent.attempt_id)
            });
        let error = normalize_remove_error(&error);
        warn!(
            action_id = %action_id,
            attempt_id = %attempt_id,
            error = %error,
            "remove-replica coordinator failed"
        );
        active.observation.action.state = DurableActionState::Failed;
        active.observation.action.error_class = Some(DurableActionErrorClass::Internal);
        active.observation.action.error = Some(error);
        self.retain_terminal(active.observation);
    }

    fn fail_before_dispatch(
        &mut self,
        mut observation: CorrelatedActionObservation,
        reply: oneshot::Sender<Result<CorrelatedControlActionAcknowledgement>>,
        message: impl Into<String>,
    ) {
        let message = message.into();
        observation.action.state = DurableActionState::Failed;
        observation.action.error_class = Some(DurableActionErrorClass::Internal);
        observation.action.error = Some(message);
        send_observation(reply, observation.clone());
        self.retain_terminal(observation);
    }

    fn dispatch_runtime(&self, execution_id: RuntimeEffectExecutionId, effect: RuntimeEffect) {
        let runtime_tx = self.runtime_tx.clone();
        let completion_tx = self.completion_tx.clone();
        tokio::spawn(async move {
            let (reply_tx, reply_rx) = oneshot::channel();
            let result = match runtime_tx
                .send(RuntimeEffectCommand {
                    effect,
                    reply: reply_tx,
                })
                .await
            {
                Ok(()) => reply_rx.await.unwrap_or_else(|_| Err(KubericError::Closed)),
                Err(_) => Err(KubericError::Closed),
            };
            let _ = completion_tx.send(RuntimeCompletion {
                execution_id,
                result,
            });
        });
    }

    fn handle_completion(&mut self, completion: RuntimeCompletion) {
        let Some(mut active) = self.active.take() else {
            warn!(
                execution_sequence = completion.execution_id.sequence,
                "discarding runtime completion without active action"
            );
            return;
        };
        if active.execution_id != completion.execution_id {
            warn!("discarding mismatched correlated runtime completion");
            self.active = Some(active);
            return;
        }

        match completion.result {
            Ok(RuntimeEffectResult::Unit) => {
                active.observation.action.state = DurableActionState::Completed;
            }
            Ok(RuntimeEffectResult::DataLoss(result)) => {
                active.observation.action.state = DurableActionState::Completed;
                active.observation.action.result = Some(DurableActionResult::DataLoss(result));
            }
            Err(error) => {
                active.observation.action.state = DurableActionState::Failed;
                active.observation.action.error_class = Some(terminal_error_class(&error));
                active.observation.action.error = Some(normalize_error(&error.to_string()));
            }
        }
        if active.observation.action.state == DurableActionState::Failed {
            warn!(
                action_id = active.observation.action.action_id,
                agent_generation = %self.generation,
                error = active.observation.action.error.as_deref().unwrap_or_default(),
                "correlated action failed"
            );
        } else {
            debug!(
                action_id = active.observation.action.action_id,
                state = ?active.observation.action.state,
                agent_generation = %self.generation,
                "correlated action reached terminal state"
            );
        }
        if let Some(reply) = active.reply {
            send_observation(reply, active.observation.clone());
        }
        self.retain_terminal(active.observation);
    }

    fn begin_shutdown(&mut self) {
        if self.shutting_down {
            return;
        }
        self.shutting_down = true;
        if let Some(authority_tx) = self
            .active
            .as_ref()
            .and_then(|active| active.coordinator_authority_tx.as_ref())
        {
            let _ = authority_tx.send(None);
        }
        info!(
            replica_id = self.replica_id,
            agent_generation = %self.generation,
            "ReplicaAgent stopping mutation admission"
        );
    }

    fn record_fault(&mut self, fault_type: FaultType) {
        self.fault_sequence = self.fault_sequence.saturating_add(1);
        if self.faults.len() == FAULT_RETENTION {
            self.faults.pop_front();
        }
        self.faults.push_back(LocalFaultRecord {
            sequence: self.fault_sequence,
            fault_type,
        });
        warn!(
            fault_sequence = self.fault_sequence,
            ?fault_type,
            agent_generation = %self.generation,
            "replica reported a local fault"
        );
    }

    fn status(&self) -> ReplicaStatusInfo {
        let runtime = self.runtime_status_rx.borrow().clone();
        let state = runtime.partition_state.as_deref();
        ReplicaStatusInfo {
            instance_id: runtime.instance_id,
            role: runtime.role,
            epoch: runtime.epoch,
            current_progress: state.map_or(0, |state| state.current_progress()),
            catch_up_capability: state.and_then(|state| state.observed_catch_up_capability()),
            committed_lsn: state.map_or(0, |state| state.committed_lsn()),
            healthy: state.is_some(),
            write_status: state.map_or(AccessStatus::NotPrimary, |state| state.write_status()),
            configuration: runtime.configuration,
            election_configuration: runtime.election_configuration,
            deactivation_info: runtime.deactivation_info,
            active_replica_connections: state
                .map_or_else(Vec::new, |state| state.active_replica_connections()),
            build_observation: runtime.build_observation,
            agent: ReplicaAgentStatus {
                protocol_version: CORRELATED_CONTROL_PROTOCOL_VERSION,
                lifecycle_peer_protocol_version: REPLICA_LIFECYCLE_PEER_PROTOCOL_VERSION,
                generation: self.generation.clone(),
                control_version: self.control_version,
                current_action: self
                    .active
                    .as_ref()
                    .map(|active| active.observation.clone()),
                retained_terminal_actions: self.terminals.iter().cloned().collect(),
                local_faults: self.faults.iter().copied().collect(),
            },
        }
    }

    fn find_correlated(&self, action_id: &str) -> Option<&CorrelatedActionObservation> {
        self.active
            .as_ref()
            .filter(|active| active.observation.action.action_id == action_id)
            .map(|active| &active.observation)
            .or_else(|| {
                self.terminals
                    .iter()
                    .rev()
                    .find(|entry| entry.action.action_id == action_id)
            })
    }

    fn retain_terminal(&mut self, observation: CorrelatedActionObservation) {
        if self.terminals.len() == TERMINAL_RETENTION {
            self.terminals.pop_front();
        }
        self.terminals.push_back(observation);
    }

    fn next_execution_id(&mut self) -> RuntimeEffectExecutionId {
        self.execution_sequence = self.execution_sequence.saturating_add(1);
        RuntimeEffectExecutionId {
            generation: self.generation.clone(),
            sequence: self.execution_sequence,
        }
    }
}

enum PeerStageRunError {
    Stale(String),
    Failed(String),
}

async fn run_peer_stage(
    request: PeerStageRequest,
    runtime_tx: mpsc::Sender<RuntimeEffectCommand>,
    runtime_status_rx: watch::Receiver<RuntimeControlSnapshot>,
    remove_clock: Arc<dyn RemoveReplicaClock>,
) -> std::result::Result<(), PeerStageRunError> {
    match request.stage {
        PeerStage::Prepare => {
            authorize_peer_sender(&request, false, remove_clock.as_ref()).await?;
            if runtime_status_rx.borrow().partition_state.is_none() {
                send_runtime_effect(
                    &runtime_tx,
                    RuntimeEffect::Open {
                        mode: crate::types::OpenMode::New,
                    },
                )
                .await?;
            }

            authorize_peer_sender(&request, false, remove_clock.as_ref()).await?;
            let current_epoch = runtime_status_rx.borrow().epoch;
            if current_epoch > request.epoch {
                return Err(PeerStageRunError::Stale(format!(
                    "target epoch {current_epoch:?} is newer than request {:?}",
                    request.epoch
                )));
            }
            if current_epoch < request.epoch {
                send_runtime_effect(
                    &runtime_tx,
                    RuntimeEffect::UpdateEpoch {
                        epoch: request.epoch,
                    },
                )
                .await?;
            }

            authorize_peer_sender(&request, false, remove_clock.as_ref()).await?;
            let runtime = runtime_status_rx.borrow().clone();
            if runtime.epoch != request.epoch {
                return Err(PeerStageRunError::Stale(
                    "target epoch did not reach peer request epoch".to_string(),
                ));
            }
            match runtime.role {
                crate::types::Role::IdleSecondary | crate::types::Role::ActiveSecondary => Ok(()),
                crate::types::Role::Unknown => {
                    drop(runtime);
                    send_runtime_effect(
                        &runtime_tx,
                        RuntimeEffect::ChangeRole {
                            epoch: request.epoch,
                            role: crate::types::Role::IdleSecondary,
                        },
                    )
                    .await
                }
                role => Err(PeerStageRunError::Stale(format!(
                    "target role {role:?} is incompatible with prepare"
                ))),
            }
        }
        PeerStage::Activate => {
            authorize_peer_sender(&request, false, remove_clock.as_ref()).await?;
            let runtime = runtime_status_rx.borrow().clone();
            if runtime.epoch != request.epoch {
                return Err(PeerStageRunError::Stale(
                    "target epoch is incompatible with activation".to_string(),
                ));
            }
            let copy_lsn = request.copy_lsn.ok_or_else(|| {
                PeerStageRunError::Failed("activation has no copy LSN".to_string())
            })?;
            let current_progress = runtime
                .partition_state
                .as_deref()
                .map_or(0, |state| state.current_progress());
            if current_progress < copy_lsn {
                return Err(PeerStageRunError::Stale(format!(
                    "target progress {current_progress} is below copy LSN {copy_lsn}"
                )));
            }
            match runtime.role {
                crate::types::Role::ActiveSecondary => Ok(()),
                crate::types::Role::IdleSecondary => {
                    drop(runtime);
                    send_runtime_effect(
                        &runtime_tx,
                        RuntimeEffect::ChangeRole {
                            epoch: request.epoch,
                            role: crate::types::Role::ActiveSecondary,
                        },
                    )
                    .await
                }
                role => Err(PeerStageRunError::Stale(format!(
                    "target role {role:?} is incompatible with activation"
                ))),
            }
        }
        PeerStage::Cleanup => {
            authorize_peer_sender(&request, true, remove_clock.as_ref()).await?;
            let runtime = runtime_status_rx.borrow().clone();
            if runtime.partition_state.is_none() {
                return Ok(());
            }
            if runtime.epoch != request.epoch {
                return Err(PeerStageRunError::Stale(
                    "target epoch is incompatible with cleanup".to_string(),
                ));
            }
            if matches!(
                runtime.role,
                crate::types::Role::IdleSecondary | crate::types::Role::ActiveSecondary
            ) {
                drop(runtime);
                send_runtime_effect(
                    &runtime_tx,
                    RuntimeEffect::ChangeRole {
                        epoch: request.epoch,
                        role: crate::types::Role::None,
                    },
                )
                .await?;
            }
            authorize_peer_sender(&request, true, remove_clock.as_ref()).await?;
            if runtime_status_rx.borrow().partition_state.is_some() {
                send_runtime_effect(&runtime_tx, RuntimeEffect::Close).await?;
            }
            Ok(())
        }
        PeerStage::Retire => {
            authorize_peer_sender(&request, false, remove_clock.as_ref()).await?;
            let runtime = runtime_status_rx.borrow().clone();
            validate_retire_target_runtime(&request, &runtime)?;
            if retire_postcondition_holds(&runtime, &request) {
                return Ok(());
            }
            if matches!(
                runtime.role,
                crate::types::Role::IdleSecondary | crate::types::Role::ActiveSecondary
            ) {
                drop(runtime);
                send_retire_runtime_effect(
                    &runtime_tx,
                    &request,
                    remove_clock.as_ref(),
                    RuntimeEffect::ChangeRole {
                        epoch: request.epoch,
                        role: crate::types::Role::None,
                    },
                )
                .await?;
            } else if runtime.role != crate::types::Role::None {
                return Err(PeerStageRunError::Stale(format!(
                    "target role {:?} is incompatible with Retire",
                    runtime.role
                )));
            }

            authorize_peer_sender(&request, false, remove_clock.as_ref()).await?;
            let runtime = runtime_status_rx.borrow().clone();
            validate_retire_target_runtime(&request, &runtime)?;
            if retire_postcondition_holds(&runtime, &request) {
                return Ok(());
            }
            if runtime.role != crate::types::Role::None {
                return Err(PeerStageRunError::Stale(
                    "target did not reach role None before Retire close".to_string(),
                ));
            }
            if runtime.partition_state.is_some() {
                send_retire_runtime_effect(
                    &runtime_tx,
                    &request,
                    remove_clock.as_ref(),
                    RuntimeEffect::Close,
                )
                .await?;
            }
            let runtime = runtime_status_rx.borrow();
            validate_retire_target_runtime(&request, &runtime)?;
            if !retire_postcondition_holds(&runtime, &request) {
                return Err(PeerStageRunError::Failed(
                    "target Retire close completed without the exact closed postcondition"
                        .to_string(),
                ));
            }
            Ok(())
        }
    }
}

async fn authorize_peer_sender(
    request: &PeerStageRequest,
    cleanup: bool,
    remove_clock: &dyn RemoveReplicaClock,
) -> std::result::Result<(), PeerStageRunError> {
    use crate::driver::ReplicaHandle;

    let authorization_budget = peer_sender_authorization_budget(request, remove_clock)?;
    let authorization_deadline = tokio::time::Instant::now() + authorization_budget;
    let handle = crate::grpc::handle::GrpcReplicaHandle::connect_control_only(
        request.sender_replica_id,
        request.sender_instance_id.clone(),
        request.sender_control_address.clone(),
        authorization_budget,
    )
    .await
    .map_err(|error| PeerStageRunError::Stale(error.to_string()))?;
    let remaining_budget =
        authorization_deadline.saturating_duration_since(tokio::time::Instant::now());
    if remaining_budget.is_zero() {
        return Err(PeerStageRunError::Stale(
            "sender authorization budget expired while connecting".to_string(),
        ));
    }
    let status = tokio::time::timeout(remaining_budget, handle.get_status())
        .await
        .map_err(|_| PeerStageRunError::Stale("sender status timed out".to_string()))?
        .map_err(|error| PeerStageRunError::Stale(error.to_string()))?;
    if status.instance_id != request.sender_instance_id
        || status.agent.generation != request.sender_agent_generation
        || status.role != crate::types::Role::Primary
        || status.epoch != request.epoch
        || status.write_status != AccessStatus::Granted
    {
        return Err(PeerStageRunError::Stale(
            "sender is not the exact active unrevoked primary".to_string(),
        ));
    }
    let parent_matches = status
        .agent
        .current_action
        .as_ref()
        .is_some_and(|observation| {
            observation.action.action_id == request.parent_action_id
                && observation.action.signature == request.parent_action_signature
                && matches!(
                    observation.action.state,
                    DurableActionState::Scheduled | DurableActionState::InProgress
                )
        });
    if !parent_matches {
        return Err(PeerStageRunError::Stale(
            "sender does not expose the exact active parent action".to_string(),
        ));
    }
    if request.stage == PeerStage::Retire {
        let configuration = status.configuration.as_ref().ok_or_else(|| {
            PeerStageRunError::Stale(
                "sender does not expose the reduced Current configuration".to_string(),
            )
        })?;
        if configuration.mode != crate::types::ReplicaConfigurationMode::Current {
            return Err(PeerStageRunError::Stale(
                "sender configuration is CatchUp rather than Current".to_string(),
            ));
        }
        if configuration.members.iter().any(|member| {
            member.id == request.target_replica_id
                && member.instance_id == request.target_instance_id
        }) {
            return Err(PeerStageRunError::Stale(
                "exact target remains in the primary Current configuration".to_string(),
            ));
        }
        if request.reduced_current_projection.as_ref() != Some(configuration) {
            return Err(PeerStageRunError::Stale(
                "sender Current configuration differs from the signed reduced projection"
                    .to_string(),
            ));
        }
    }
    if cleanup
        && status.configuration.as_ref().is_some_and(|configuration| {
            configuration.mode == crate::types::ReplicaConfigurationMode::Current
                && configuration.members.iter().any(|member| {
                    member.id == request.target_replica_id
                        && member.instance_id == request.target_instance_id
                })
        })
    {
        return Err(PeerStageRunError::Stale(
            "target is already present in the primary current configuration".to_string(),
        ));
    }
    Ok(())
}

fn peer_sender_authorization_budget(
    request: &PeerStageRequest,
    remove_clock: &dyn RemoveReplicaClock,
) -> std::result::Result<std::time::Duration, PeerStageRunError> {
    if request.stage != PeerStage::Retire {
        return Ok(std::time::Duration::from_secs(5));
    }
    let expiry = request.retirement_expiry_unix_seconds.ok_or_else(|| {
        PeerStageRunError::Stale("Retire stage has no signed retirement expiry".to_string())
    })?;
    let remaining = expiry.saturating_sub(remove_clock.unix_seconds());
    if remaining <= 0 {
        return Err(PeerStageRunError::Stale(
            "Retire stage deadline has expired".to_string(),
        ));
    }
    Ok(
        std::time::Duration::from_secs(remaining as u64).min(std::time::Duration::from_secs(
            REMOVE_REPLICA_CALL_TIMEOUT_SECONDS as u64,
        )),
    )
}

fn validate_retire_target_runtime(
    request: &PeerStageRequest,
    runtime: &RuntimeControlSnapshot,
) -> std::result::Result<(), PeerStageRunError> {
    if runtime.instance_id != request.target_instance_id {
        return Err(PeerStageRunError::Stale(
            "target runtime incarnation changed during Retire".to_string(),
        ));
    }
    if runtime.epoch != request.epoch {
        return Err(PeerStageRunError::Stale(
            "target runtime epoch changed during Retire".to_string(),
        ));
    }
    Ok(())
}

fn retire_postcondition_holds(
    runtime: &RuntimeControlSnapshot,
    request: &PeerStageRequest,
) -> bool {
    runtime.instance_id == request.target_instance_id
        && runtime.epoch == request.epoch
        && runtime.role == crate::types::Role::None
        && runtime.partition_state.is_none()
}

async fn send_runtime_effect(
    runtime_tx: &mpsc::Sender<RuntimeEffectCommand>,
    effect: RuntimeEffect,
) -> std::result::Result<(), PeerStageRunError> {
    let (reply_tx, reply_rx) = oneshot::channel();
    runtime_tx
        .send(RuntimeEffectCommand {
            effect,
            reply: reply_tx,
        })
        .await
        .map_err(|_| PeerStageRunError::Failed("target runtime closed".to_string()))?;
    match reply_rx.await {
        Ok(Ok(RuntimeEffectResult::Unit)) => Ok(()),
        Ok(Ok(RuntimeEffectResult::DataLoss(_))) => Err(PeerStageRunError::Failed(
            "unexpected data-loss result for peer stage".to_string(),
        )),
        Ok(Err(error)) => Err(PeerStageRunError::Failed(error.to_string())),
        Err(_) => Err(PeerStageRunError::Failed(
            "target runtime reply closed".to_string(),
        )),
    }
}

async fn send_retire_runtime_effect(
    runtime_tx: &mpsc::Sender<RuntimeEffectCommand>,
    request: &PeerStageRequest,
    remove_clock: &dyn RemoveReplicaClock,
    effect: RuntimeEffect,
) -> std::result::Result<(), PeerStageRunError> {
    let timeout = peer_sender_authorization_budget(request, remove_clock)?;
    tokio::time::timeout(timeout, send_runtime_effect(runtime_tx, effect))
        .await
        .map_err(|_| {
            PeerStageRunError::Failed("target Retire runtime effect timed out".to_string())
        })?
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RemoveConfigurationObservation {
    PreviousCurrent,
    ReducedCatchUp,
    ReducedCurrent,
}

struct RemoveCoordinatorState {
    progress: RemoveReplicaProgress,
    current_install_dispatched: bool,
}

enum PreCommitOutcome {
    Committed,
    Failed(String),
}

enum CompensationOutcome {
    Committed,
    Terminal(RemoveReplicaTerminalResult),
}

#[allow(clippy::too_many_arguments)]
async fn run_remove_replica_coordinator(
    execution: CoordinatorExecution,
    intent: RemoveReplicaIntent,
    runtime_tx: mpsc::Sender<RuntimeEffectCommand>,
    mut runtime_status_rx: watch::Receiver<RuntimeControlSnapshot>,
    mut authority_rx: watch::Receiver<Option<CoordinatorAuthority>>,
    lifecycle_peer_transport: Arc<dyn LifecyclePeerTransport>,
    remove_clock: Arc<dyn RemoveReplicaClock>,
    coordinator_tx: mpsc::UnboundedSender<CoordinatorEvent>,
) {
    let mut state = RemoveCoordinatorState {
        progress: RemoveReplicaProgress {
            phase: RemoveReplicaCoordinatorPhase::Validating,
            attempt_id: intent.attempt_id.clone(),
            commit_observed: false,
            commit_observed_unix_seconds: None,
            connection_absent: false,
            target_retirement: TargetRetirementObservation::NotAttempted,
            retirement_expiry_unix_seconds: None,
            compensation_expiry_unix_seconds: None,
            error: None,
            current_install_dispatched: false,
        },
        current_install_dispatched: false,
    };

    let initial_configuration = {
        let runtime = runtime_status_rx.borrow();
        classify_remove_configuration(&runtime, &intent)
    };
    let terminal = match initial_configuration {
        Ok(RemoveConfigurationObservation::ReducedCurrent) => {
            match observe_remove_commit(
                &execution,
                &intent,
                &mut state,
                &mut authority_rx,
                &runtime_status_rx,
                remove_clock.as_ref(),
                &coordinator_tx,
            )
            .await
            {
                Ok(()) => {
                    run_remove_post_commit(
                        &execution,
                        &intent,
                        &mut state,
                        &runtime_tx,
                        &mut runtime_status_rx,
                        &mut authority_rx,
                        lifecycle_peer_transport.as_ref(),
                        remove_clock.as_ref(),
                        &coordinator_tx,
                    )
                    .await
                }
                Err(error) => Err(error),
            }
        }
        Ok(RemoveConfigurationObservation::PreviousCurrent)
        | Ok(RemoveConfigurationObservation::ReducedCatchUp) => {
            match run_remove_pre_commit(
                &execution,
                &intent,
                &mut state,
                &runtime_tx,
                &mut runtime_status_rx,
                &mut authority_rx,
                remove_clock.as_ref(),
                &coordinator_tx,
            )
            .await
            {
                PreCommitOutcome::Committed => {
                    run_remove_post_commit(
                        &execution,
                        &intent,
                        &mut state,
                        &runtime_tx,
                        &mut runtime_status_rx,
                        &mut authority_rx,
                        lifecycle_peer_transport.as_ref(),
                        remove_clock.as_ref(),
                        &coordinator_tx,
                    )
                    .await
                }
                PreCommitOutcome::Failed(error) => {
                    warn!(
                        action_id = execution.action_id,
                        attempt_id = execution.attempt_id,
                        error = %normalize_remove_error(&error),
                        current_install_dispatched = state.current_install_dispatched,
                        "remove-replica coordinator left the pre-commit path"
                    );
                    match recover_remove_pre_commit_failure(
                        &execution,
                        &intent,
                        &mut state,
                        &runtime_tx,
                        &mut runtime_status_rx,
                        &mut authority_rx,
                        lifecycle_peer_transport.as_ref(),
                        remove_clock.as_ref(),
                        &coordinator_tx,
                        error,
                    )
                    .await
                    {
                        Ok(CompensationOutcome::Committed) => {
                            run_remove_post_commit(
                                &execution,
                                &intent,
                                &mut state,
                                &runtime_tx,
                                &mut runtime_status_rx,
                                &mut authority_rx,
                                lifecycle_peer_transport.as_ref(),
                                remove_clock.as_ref(),
                                &coordinator_tx,
                            )
                            .await
                        }
                        Ok(CompensationOutcome::Terminal(result)) => Ok(result),
                        Err(error) => Err(error),
                    }
                }
            }
        }
        Err(error) => Err(error),
    };

    if let Ok(result) = &terminal
        && let Err(error) = intent.validate_terminal_progress(&state.progress, *result)
    {
        let _ = coordinator_tx.send(CoordinatorEvent {
            execution_id: execution.current_id(),
            update: CoordinatorUpdate::RemoveTerminal(Err(format!(
                "remove-replica coordinator produced invalid terminal state: {error}"
            ))),
        });
        return;
    }
    let _ = coordinator_tx.send(CoordinatorEvent {
        execution_id: execution.current_id(),
        update: CoordinatorUpdate::RemoveTerminal(terminal),
    });
}

#[allow(clippy::too_many_arguments)]
async fn run_remove_pre_commit(
    execution: &CoordinatorExecution,
    intent: &RemoveReplicaIntent,
    state: &mut RemoveCoordinatorState,
    runtime_tx: &mpsc::Sender<RuntimeEffectCommand>,
    runtime_status_rx: &mut watch::Receiver<RuntimeControlSnapshot>,
    authority_rx: &mut watch::Receiver<Option<CoordinatorAuthority>>,
    remove_clock: &dyn RemoveReplicaClock,
    coordinator_tx: &mpsc::UnboundedSender<CoordinatorEvent>,
) -> PreCommitOutcome {
    let result = async {
        transition_remove_progress(
            execution,
            intent,
            state,
            authority_rx,
            coordinator_tx,
            RemoveReplicaCoordinatorPhase::InstallingCatchUpConfiguration,
        )
        .await?;
        let configuration = remove_authority_gate(
            execution,
            intent,
            authority_rx,
            runtime_status_rx,
            remove_clock,
            Some(intent.overall_deadline_unix_seconds),
            &[
                RemoveConfigurationObservation::PreviousCurrent,
                RemoveConfigurationObservation::ReducedCatchUp,
                RemoveConfigurationObservation::ReducedCurrent,
            ],
        )
        .await?;
        if configuration == RemoveConfigurationObservation::ReducedCurrent {
            observe_remove_commit(
                execution,
                intent,
                state,
                authority_rx,
                runtime_status_rx,
                remove_clock,
                coordinator_tx,
            )
            .await?;
            return Ok(());
        }
        if configuration == RemoveConfigurationObservation::PreviousCurrent {
            let effect_result = execute_remove_runtime_effect(
                runtime_tx,
                RuntimeEffect::UpdateCatchUpConfiguration {
                    current: intent.reduced_catch_up_configuration.materialize(None)?,
                    previous: intent.previous_configuration.materialize(None)?,
                },
                remove_clock,
                intent.overall_deadline_unix_seconds,
                intent.call_timeout_seconds,
                "install reduced catch-up configuration",
            )
            .await;
            let observed = remove_authority_gate(
                execution,
                intent,
                authority_rx,
                runtime_status_rx,
                remove_clock,
                Some(intent.overall_deadline_unix_seconds),
                &[
                    RemoveConfigurationObservation::PreviousCurrent,
                    RemoveConfigurationObservation::ReducedCatchUp,
                    RemoveConfigurationObservation::ReducedCurrent,
                ],
            )
            .await?;
            if observed == RemoveConfigurationObservation::ReducedCurrent {
                observe_remove_commit(
                    execution,
                    intent,
                    state,
                    authority_rx,
                    runtime_status_rx,
                    remove_clock,
                    coordinator_tx,
                )
                .await?;
                return Ok(());
            }
            if observed != RemoveConfigurationObservation::ReducedCatchUp {
                return Err(effect_result.err().unwrap_or_else(|| {
                    "reduced catch-up effect completed without its exact postcondition".to_string()
                }));
            }
        }

        transition_remove_progress(
            execution,
            intent,
            state,
            authority_rx,
            coordinator_tx,
            RemoveReplicaCoordinatorPhase::WaitingForCatchUpQuorum,
        )
        .await?;
        let wait_id = format!("{}:remove-quorum", intent.attempt_id);
        loop {
            let configuration = remove_authority_gate(
                execution,
                intent,
                authority_rx,
                runtime_status_rx,
                remove_clock,
                Some(intent.overall_deadline_unix_seconds),
                &[
                    RemoveConfigurationObservation::ReducedCatchUp,
                    RemoveConfigurationObservation::ReducedCurrent,
                ],
            )
            .await?;
            if configuration == RemoveConfigurationObservation::ReducedCurrent {
                observe_remove_commit(
                    execution,
                    intent,
                    state,
                    authority_rx,
                    runtime_status_rx,
                    remove_clock,
                    coordinator_tx,
                )
                .await?;
                return Ok(());
            }
            let observation = runtime_status_rx.borrow().quorum_wait_observation.clone();
            match observation.as_ref().filter(|value| value.execution_id == wait_id) {
                Some(observation) => match observation.state {
                    crate::add_replica::RuntimeBuildState::Completed => break,
                    crate::add_replica::RuntimeBuildState::Failed
                    | crate::add_replica::RuntimeBuildState::Cancelled => {
                        return Err(observation.error.clone().unwrap_or_else(|| {
                            "tracked remove-replica quorum wait failed".to_string()
                        }));
                    }
                    crate::add_replica::RuntimeBuildState::InProgress => {}
                },
                None => {
                    execute_remove_runtime_effect(
                        runtime_tx,
                        RuntimeEffect::StartTrackedCatchUpQuorum {
                            execution_id: wait_id.clone(),
                            mode: ReplicaSetQuorumMode::Write,
                        },
                        remove_clock,
                        intent.overall_deadline_unix_seconds,
                        intent.call_timeout_seconds,
                        "start tracked remove-replica quorum wait",
                    )
                    .await?;
                    remove_authority_gate(
                        execution,
                        intent,
                        authority_rx,
                        runtime_status_rx,
                        remove_clock,
                        Some(intent.overall_deadline_unix_seconds),
                        &[
                            RemoveConfigurationObservation::ReducedCatchUp,
                            RemoveConfigurationObservation::ReducedCurrent,
                        ],
                    )
                    .await?;
                }
            }
            wait_for_remove_observation(runtime_status_rx, authority_rx).await?;
        }

        transition_remove_progress(
            execution,
            intent,
            state,
            authority_rx,
            coordinator_tx,
            RemoveReplicaCoordinatorPhase::InstallingCurrentConfiguration,
        )
        .await?;
        let configuration = remove_authority_gate(
            execution,
            intent,
            authority_rx,
            runtime_status_rx,
            remove_clock,
            Some(intent.overall_deadline_unix_seconds),
            &[
                RemoveConfigurationObservation::ReducedCatchUp,
                RemoveConfigurationObservation::ReducedCurrent,
            ],
        )
        .await?;
        if configuration != RemoveConfigurationObservation::ReducedCurrent {
            state.current_install_dispatched = true;
            state.progress.current_install_dispatched = true;
            publish_remove_progress(
                execution,
                intent,
                state,
                authority_rx,
                coordinator_tx,
            )
            .await?;
            let effect_result = execute_remove_runtime_effect(
                runtime_tx,
                RuntimeEffect::UpdateCurrentConfiguration {
                    current: intent.reduced_current_configuration.materialize(None)?,
                },
                remove_clock,
                intent.overall_deadline_unix_seconds,
                intent.call_timeout_seconds,
                "install reduced current configuration",
            )
            .await;
            let observed = remove_authority_gate(
                execution,
                intent,
                authority_rx,
                runtime_status_rx,
                remove_clock,
                Some(intent.overall_deadline_unix_seconds),
                &[
                    RemoveConfigurationObservation::ReducedCatchUp,
                    RemoveConfigurationObservation::ReducedCurrent,
                ],
            )
            .await;
            if observed != Ok(RemoveConfigurationObservation::ReducedCurrent) {
                return Err(effect_result.err().unwrap_or_else(|| {
                    observed.err().unwrap_or_else(|| {
                        "reduced-current installation was dispatched but its exact outcome is ambiguous"
                            .to_string()
                    })
                }));
            }
        }
        observe_remove_commit(
            execution,
            intent,
            state,
            authority_rx,
            runtime_status_rx,
            remove_clock,
            coordinator_tx,
        )
        .await
    }
    .await;

    match result {
        Ok(()) => PreCommitOutcome::Committed,
        Err(error) => PreCommitOutcome::Failed(normalize_remove_error(&error)),
    }
}

#[allow(clippy::too_many_arguments)]
async fn recover_remove_pre_commit_failure(
    execution: &CoordinatorExecution,
    intent: &RemoveReplicaIntent,
    state: &mut RemoveCoordinatorState,
    runtime_tx: &mpsc::Sender<RuntimeEffectCommand>,
    runtime_status_rx: &mut watch::Receiver<RuntimeControlSnapshot>,
    authority_rx: &mut watch::Receiver<Option<CoordinatorAuthority>>,
    _lifecycle_peer_transport: &dyn LifecyclePeerTransport,
    remove_clock: &dyn RemoveReplicaClock,
    coordinator_tx: &mpsc::UnboundedSender<CoordinatorEvent>,
    failure: String,
) -> std::result::Result<CompensationOutcome, String> {
    let observed = {
        let runtime = runtime_status_rx.borrow();
        classify_remove_configuration(&runtime, intent)
    };
    match observed {
        Ok(RemoveConfigurationObservation::ReducedCurrent) => {
            observe_remove_commit(
                execution,
                intent,
                state,
                authority_rx,
                runtime_status_rx,
                remove_clock,
                coordinator_tx,
            )
            .await?;
            return Ok(CompensationOutcome::Committed);
        }
        Ok(RemoveConfigurationObservation::PreviousCurrent) if state.current_install_dispatched => {
            enter_remove_compensation(
                execution,
                intent,
                state,
                authority_rx,
                coordinator_tx,
                remove_clock,
                &failure,
            )
            .await?;
            return Ok(CompensationOutcome::Terminal(
                RemoveReplicaTerminalResult::Compensated,
            ));
        }
        Ok(_) if state.current_install_dispatched => {
            return Err(normalize_remove_error(
                "reduced-current installation was dispatched and exact commit state is ambiguous; previous-configuration rollback is forbidden",
            ));
        }
        Err(error) if state.current_install_dispatched => {
            return Err(normalize_remove_error(&format!(
                "reduced-current installation was dispatched and configuration observation is invalid; previous-configuration rollback is forbidden: {error}"
            )));
        }
        _ => {}
    }

    enter_remove_compensation(
        execution,
        intent,
        state,
        authority_rx,
        coordinator_tx,
        remove_clock,
        &failure,
    )
    .await?;
    let compensation_expiry = state
        .progress
        .compensation_expiry_unix_seconds
        .ok_or_else(|| "remove-replica compensation has no derived expiry".to_string())?;
    let wait_id = format!("{}:remove-quorum", intent.attempt_id);

    loop {
        if remove_clock.unix_seconds() >= compensation_expiry {
            state.progress.error = Some(normalize_remove_error(
                state
                    .progress
                    .error
                    .as_deref()
                    .unwrap_or("remove-replica compensation expired before restoration"),
            ));
            publish_remove_progress(execution, intent, state, authority_rx, coordinator_tx).await?;
            return Ok(CompensationOutcome::Terminal(
                RemoveReplicaTerminalResult::CompensationIncomplete,
            ));
        }

        let configuration = match remove_authority_gate(
            execution,
            intent,
            authority_rx,
            runtime_status_rx,
            remove_clock,
            Some(compensation_expiry),
            &[
                RemoveConfigurationObservation::PreviousCurrent,
                RemoveConfigurationObservation::ReducedCatchUp,
                RemoveConfigurationObservation::ReducedCurrent,
            ],
        )
        .await
        {
            Ok(configuration) => configuration,
            Err(error) => {
                state.progress.error = Some(normalize_remove_error(&error));
                wait_for_remove_observation(runtime_status_rx, authority_rx).await?;
                continue;
            }
        };
        match configuration {
            RemoveConfigurationObservation::ReducedCurrent => {
                observe_remove_commit(
                    execution,
                    intent,
                    state,
                    authority_rx,
                    runtime_status_rx,
                    remove_clock,
                    coordinator_tx,
                )
                .await?;
                return Ok(CompensationOutcome::Committed);
            }
            RemoveConfigurationObservation::PreviousCurrent => {
                return Ok(CompensationOutcome::Terminal(
                    RemoveReplicaTerminalResult::Compensated,
                ));
            }
            RemoveConfigurationObservation::ReducedCatchUp => {}
        }

        let quorum_in_progress = runtime_status_rx
            .borrow()
            .quorum_wait_observation
            .as_ref()
            .is_some_and(|observation| {
                observation.execution_id == wait_id
                    && observation.state == crate::add_replica::RuntimeBuildState::InProgress
            });
        if quorum_in_progress {
            let _ = execute_remove_runtime_effect(
                runtime_tx,
                RuntimeEffect::CancelTrackedOperation {
                    execution_id: wait_id.clone(),
                },
                remove_clock,
                compensation_expiry,
                intent.call_timeout_seconds,
                "cancel tracked remove-replica quorum wait",
            )
            .await;
            remove_authority_gate(
                execution,
                intent,
                authority_rx,
                runtime_status_rx,
                remove_clock,
                Some(compensation_expiry),
                &[
                    RemoveConfigurationObservation::PreviousCurrent,
                    RemoveConfigurationObservation::ReducedCatchUp,
                    RemoveConfigurationObservation::ReducedCurrent,
                ],
            )
            .await?;
            wait_for_remove_observation(runtime_status_rx, authority_rx).await?;
            continue;
        }

        let effect_result = execute_remove_runtime_effect(
            runtime_tx,
            RuntimeEffect::UpdateCurrentConfiguration {
                current: intent.previous_configuration.materialize(None)?,
            },
            remove_clock,
            compensation_expiry,
            intent.call_timeout_seconds,
            "restore previous current configuration",
        )
        .await;
        let observed = remove_authority_gate(
            execution,
            intent,
            authority_rx,
            runtime_status_rx,
            remove_clock,
            Some(compensation_expiry),
            &[
                RemoveConfigurationObservation::PreviousCurrent,
                RemoveConfigurationObservation::ReducedCatchUp,
                RemoveConfigurationObservation::ReducedCurrent,
            ],
        )
        .await;
        match observed {
            Ok(RemoveConfigurationObservation::PreviousCurrent) => {
                return Ok(CompensationOutcome::Terminal(
                    RemoveReplicaTerminalResult::Compensated,
                ));
            }
            Ok(RemoveConfigurationObservation::ReducedCurrent) => {
                observe_remove_commit(
                    execution,
                    intent,
                    state,
                    authority_rx,
                    runtime_status_rx,
                    remove_clock,
                    coordinator_tx,
                )
                .await?;
                return Ok(CompensationOutcome::Committed);
            }
            _ => {
                if let Err(error) = effect_result {
                    state.progress.error = Some(normalize_remove_error(&error));
                }
                wait_for_remove_observation(runtime_status_rx, authority_rx).await?;
            }
        }
    }
}

async fn enter_remove_compensation(
    execution: &CoordinatorExecution,
    intent: &RemoveReplicaIntent,
    state: &mut RemoveCoordinatorState,
    authority_rx: &mut watch::Receiver<Option<CoordinatorAuthority>>,
    coordinator_tx: &mpsc::UnboundedSender<CoordinatorEvent>,
    remove_clock: &dyn RemoveReplicaClock,
    failure: &str,
) -> std::result::Result<(), String> {
    if state.progress.compensation_expiry_unix_seconds.is_none() {
        let observed = remove_clock
            .unix_seconds()
            .min(intent.compensation_deadline_cap_unix_seconds);
        state.progress.compensation_expiry_unix_seconds =
            Some(intent.compensation_expiry(observed)?);
    }
    state.progress.error = Some(normalize_remove_error(failure));
    transition_remove_progress(
        execution,
        intent,
        state,
        authority_rx,
        coordinator_tx,
        RemoveReplicaCoordinatorPhase::Compensating,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn observe_remove_commit(
    execution: &CoordinatorExecution,
    intent: &RemoveReplicaIntent,
    state: &mut RemoveCoordinatorState,
    authority_rx: &mut watch::Receiver<Option<CoordinatorAuthority>>,
    runtime_status_rx: &watch::Receiver<RuntimeControlSnapshot>,
    remove_clock: &dyn RemoveReplicaClock,
    coordinator_tx: &mpsc::UnboundedSender<CoordinatorEvent>,
) -> std::result::Result<(), String> {
    remove_authority_gate(
        execution,
        intent,
        authority_rx,
        runtime_status_rx,
        remove_clock,
        None,
        &[RemoveConfigurationObservation::ReducedCurrent],
    )
    .await?;
    if state.progress.commit_observed {
        return Ok(());
    }
    let commit_observed = remove_clock
        .unix_seconds()
        .min(intent.overall_deadline_unix_seconds);
    let retirement_expiry = intent.retirement_expiry(commit_observed)?;
    state.progress.commit_observed = true;
    state.progress.commit_observed_unix_seconds = Some(commit_observed);
    state.progress.retirement_expiry_unix_seconds = Some(retirement_expiry);
    state.progress.compensation_expiry_unix_seconds = None;
    state.progress.error = None;
    transition_remove_progress(
        execution,
        intent,
        state,
        authority_rx,
        coordinator_tx,
        RemoveReplicaCoordinatorPhase::RemovingConnection,
    )
    .await?;
    info!(
        action_id = execution.action_id,
        attempt_id = execution.attempt_id,
        commit_observed_unix_seconds = commit_observed,
        retirement_expiry_unix_seconds = retirement_expiry,
        "remove-replica reduced Current configuration committed"
    );
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn run_remove_post_commit(
    execution: &CoordinatorExecution,
    intent: &RemoveReplicaIntent,
    state: &mut RemoveCoordinatorState,
    runtime_tx: &mpsc::Sender<RuntimeEffectCommand>,
    runtime_status_rx: &mut watch::Receiver<RuntimeControlSnapshot>,
    authority_rx: &mut watch::Receiver<Option<CoordinatorAuthority>>,
    lifecycle_peer_transport: &dyn LifecyclePeerTransport,
    remove_clock: &dyn RemoveReplicaClock,
    coordinator_tx: &mpsc::UnboundedSender<CoordinatorEvent>,
) -> std::result::Result<RemoveReplicaTerminalResult, String> {
    if !state.progress.commit_observed {
        return Err("post-commit remove path has no commit evidence".to_string());
    }
    loop {
        remove_authority_gate(
            execution,
            intent,
            authority_rx,
            runtime_status_rx,
            remove_clock,
            None,
            &[RemoveConfigurationObservation::ReducedCurrent],
        )
        .await?;
        if remove_connection_absent(runtime_status_rx, intent) {
            state.progress.connection_absent = true;
            publish_remove_progress(execution, intent, state, authority_rx, coordinator_tx).await?;
            break;
        }
        let call_deadline = remove_clock
            .unix_seconds()
            .saturating_add(intent.call_timeout_seconds);
        let result = execute_remove_runtime_effect(
            runtime_tx,
            RuntimeEffect::RemoveReplica {
                replica_id: intent.target_replica_id,
                instance_id: intent.target_instance_id.clone(),
            },
            remove_clock,
            call_deadline,
            intent.call_timeout_seconds,
            "remove exact target connection",
        )
        .await;
        remove_authority_gate(
            execution,
            intent,
            authority_rx,
            runtime_status_rx,
            remove_clock,
            None,
            &[RemoveConfigurationObservation::ReducedCurrent],
        )
        .await?;
        if remove_connection_absent(runtime_status_rx, intent) {
            state.progress.connection_absent = true;
            publish_remove_progress(execution, intent, state, authority_rx, coordinator_tx).await?;
            break;
        }
        if let Err(error) = result {
            state.progress.error = Some(normalize_remove_error(&error));
            warn!(
                action_id = execution.action_id,
                attempt_id = execution.attempt_id,
                error = %normalize_remove_error(&error),
                "remove-replica exact connection cleanup will retry"
            );
        }
        wait_for_remove_observation(runtime_status_rx, authority_rx).await?;
    }

    transition_remove_progress(
        execution,
        intent,
        state,
        authority_rx,
        coordinator_tx,
        RemoveReplicaCoordinatorPhase::RetiringTarget,
    )
    .await?;
    let retirement = ensure_remove_target_retirement(
        execution,
        intent,
        state,
        runtime_status_rx,
        authority_rx,
        lifecycle_peer_transport,
        remove_clock,
        coordinator_tx,
    )
    .await?;
    state.progress.target_retirement = retirement;
    state.progress.error = match retirement {
        TargetRetirementObservation::Completed => None,
        TargetRetirementObservation::Unavailable => {
            Some("target retirement unavailable before its signed expiry".to_string())
        }
        TargetRetirementObservation::Stale => {
            Some("target retirement became stale before its signed expiry".to_string())
        }
        TargetRetirementObservation::Failed => {
            Some("target retirement failed before its signed expiry".to_string())
        }
        TargetRetirementObservation::NotAttempted | TargetRetirementObservation::InProgress => {
            return Err("target retirement ended without a terminal observation".to_string());
        }
    };
    transition_remove_progress(
        execution,
        intent,
        state,
        authority_rx,
        coordinator_tx,
        RemoveReplicaCoordinatorPhase::Attesting,
    )
    .await?;
    remove_authority_gate(
        execution,
        intent,
        authority_rx,
        runtime_status_rx,
        remove_clock,
        None,
        &[RemoveConfigurationObservation::ReducedCurrent],
    )
    .await?;
    if !remove_connection_absent(runtime_status_rx, intent) {
        return Err("remove-replica final attestation found the old connection".to_string());
    }
    let result = if retirement == TargetRetirementObservation::Completed {
        RemoveReplicaTerminalResult::CommittedClean
    } else {
        warn!(
            action_id = execution.action_id,
            attempt_id = execution.attempt_id,
            retirement = ?retirement,
            "remove-replica committed with degraded target retirement"
        );
        RemoveReplicaTerminalResult::CommittedDegraded
    };
    state.progress.validate_terminal(result)?;
    Ok(result)
}

#[allow(clippy::too_many_arguments)]
async fn ensure_remove_target_retirement(
    execution: &CoordinatorExecution,
    intent: &RemoveReplicaIntent,
    state: &mut RemoveCoordinatorState,
    runtime_status_rx: &watch::Receiver<RuntimeControlSnapshot>,
    authority_rx: &mut watch::Receiver<Option<CoordinatorAuthority>>,
    lifecycle_peer_transport: &dyn LifecyclePeerTransport,
    remove_clock: &dyn RemoveReplicaClock,
    coordinator_tx: &mpsc::UnboundedSender<CoordinatorEvent>,
) -> std::result::Result<TargetRetirementObservation, String> {
    let has_peer_authority = intent.expected_target_agent_generation.is_some()
        && intent.target_control_address.is_some()
        && intent.target_replicator_address.is_some()
        && intent.target_lifecycle_peer_protocol_version
            == Some(REPLICA_LIFECYCLE_PEER_PROTOCOL_VERSION);
    if intent.mode == RemoveReplicaMode::Force && !has_peer_authority {
        state.progress.target_retirement = TargetRetirementObservation::Unavailable;
        publish_remove_progress(execution, intent, state, authority_rx, coordinator_tx).await?;
        return Ok(TargetRetirementObservation::Unavailable);
    }
    if !has_peer_authority {
        return Err("scale-down removal lost its frozen target peer authority".to_string());
    }
    let expiry = state
        .progress
        .retirement_expiry_unix_seconds
        .ok_or_else(|| "committed removal has no retirement expiry".to_string())?;
    let mut last_observation = TargetRetirementObservation::Unavailable;
    let message_id = format!("{}:Retire", intent.attempt_id);

    loop {
        if remove_clock.unix_seconds() >= expiry {
            return Ok(match last_observation {
                TargetRetirementObservation::NotAttempted
                | TargetRetirementObservation::InProgress => TargetRetirementObservation::Failed,
                terminal => terminal,
            });
        }
        remove_authority_gate(
            execution,
            intent,
            authority_rx,
            runtime_status_rx,
            remove_clock,
            Some(expiry),
            &[RemoveConfigurationObservation::ReducedCurrent],
        )
        .await?;
        let call_deadline =
            remove_call_deadline(remove_clock, expiry, intent.call_timeout_seconds)?;
        let timeout = remove_timeout_duration(remove_clock, call_deadline)?;
        let status = await_remove_clock_bounded(
            lifecycle_peer_transport.get_status(intent, timeout),
            remove_clock,
            call_deadline,
        )
        .await;
        if remove_clock.unix_seconds() >= expiry {
            return Ok(match last_observation {
                TargetRetirementObservation::NotAttempted
                | TargetRetirementObservation::InProgress => TargetRetirementObservation::Failed,
                terminal => terminal,
            });
        }
        remove_authority_gate(
            execution,
            intent,
            authority_rx,
            runtime_status_rx,
            remove_clock,
            Some(expiry),
            &[RemoveConfigurationObservation::ReducedCurrent],
        )
        .await?;
        let status = match status {
            Ok(Ok(status)) => status,
            Ok(Err(error)) => {
                last_observation = retirement_observation_from_transport_error(&error);
                state.progress.target_retirement = last_observation;
                state.progress.error = Some(error.message);
                publish_remove_progress(execution, intent, state, authority_rx, coordinator_tx)
                    .await?;
                wait_for_remove_observation_only(authority_rx).await?;
                continue;
            }
            Err(()) => {
                last_observation = TargetRetirementObservation::Unavailable;
                state.progress.target_retirement = last_observation;
                state.progress.error = Some("target lifecycle status call timed out".to_string());
                publish_remove_progress(execution, intent, state, authority_rx, coordinator_tx)
                    .await?;
                continue;
            }
        };
        if let Err(error) = validate_remove_peer_status(intent, &status) {
            last_observation = TargetRetirementObservation::Stale;
            state.progress.target_retirement = last_observation;
            state.progress.error = Some(normalize_remove_error(&error));
            publish_remove_progress(execution, intent, state, authority_rx, coordinator_tx).await?;
            wait_for_remove_observation_only(authority_rx).await?;
            continue;
        }
        if !status.healthy && status.role == Role::None {
            return Ok(TargetRetirementObservation::Completed);
        }
        if let Some(observation) = status
            .current_action
            .as_ref()
            .into_iter()
            .chain(status.retained_terminal_actions.iter().rev())
            .find(|observation| observation.message_id == message_id)
        {
            match observation.state {
                PeerStageState::Accepted
                | PeerStageState::InProgress
                | PeerStageState::Completed => {
                    last_observation = TargetRetirementObservation::InProgress;
                    state.progress.target_retirement = last_observation;
                    state.progress.error = None;
                    publish_remove_progress(execution, intent, state, authority_rx, coordinator_tx)
                        .await?;
                    wait_for_remove_observation_only(authority_rx).await?;
                    continue;
                }
                PeerStageState::Stale => {
                    last_observation = TargetRetirementObservation::Stale;
                }
                PeerStageState::Failed | PeerStageState::Rejected | PeerStageState::Conflict => {
                    last_observation = TargetRetirementObservation::Failed;
                }
            }
            state.progress.target_retirement = last_observation;
            state.progress.error = observation.error.as_deref().map(normalize_remove_error);
            publish_remove_progress(execution, intent, state, authority_rx, coordinator_tx).await?;
            wait_for_remove_observation_only(authority_rx).await?;
            continue;
        }

        let mut request = PeerStageRequest {
            protocol_version: REPLICA_LIFECYCLE_PEER_PROTOCOL_VERSION,
            operation_kind: PeerOperationKind::Remove,
            stage_semantic_version: crate::replica_lifecycle::RETIRE_STAGE_SEMANTIC_VERSION,
            operation_id: intent.operation_id.clone(),
            attempt_id: intent.attempt_id.clone(),
            message_id: message_id.clone(),
            input_signature: String::new(),
            stage: PeerStage::Retire,
            sender_replica_id: intent.primary_replica_id,
            sender_instance_id: intent.primary_instance_id.clone(),
            sender_agent_generation: intent.primary_agent_generation.clone(),
            sender_control_address: intent.primary_control_address.clone(),
            parent_action_id: intent.action_id.clone(),
            parent_action_signature: intent.input_signature.clone(),
            target_replica_id: intent.target_replica_id,
            target_instance_id: intent.target_instance_id.clone(),
            expected_target_agent_generation: intent
                .expected_target_agent_generation
                .clone()
                .ok_or_else(|| "remove-replica target generation is missing".to_string())?,
            expected_target_peer_control_version: status.peer_control_version,
            epoch: intent.epoch,
            configuration_fence: intent.configuration_fence(),
            build_key: None,
            copy_lsn: None,
            removal_mode: Some(intent.mode),
            commit_observed_unix_seconds: state.progress.commit_observed_unix_seconds,
            retirement_expiry_unix_seconds: Some(expiry),
            reduced_current_projection: Some(intent.reduced_current_status()),
        };
        request.input_signature = request.signature();
        request.validate()?;
        let call_deadline =
            remove_call_deadline(remove_clock, expiry, intent.call_timeout_seconds)?;
        let timeout = remove_timeout_duration(remove_clock, call_deadline)?;
        let stage_result = await_remove_clock_bounded(
            lifecycle_peer_transport.execute_stage(intent, request, timeout),
            remove_clock,
            call_deadline,
        )
        .await;
        if remove_clock.unix_seconds() >= expiry {
            return Ok(match last_observation {
                TargetRetirementObservation::NotAttempted
                | TargetRetirementObservation::InProgress => TargetRetirementObservation::Failed,
                terminal => terminal,
            });
        }
        remove_authority_gate(
            execution,
            intent,
            authority_rx,
            runtime_status_rx,
            remove_clock,
            Some(expiry),
            &[RemoveConfigurationObservation::ReducedCurrent],
        )
        .await?;
        match stage_result {
            Ok(Ok(observation)) => {
                last_observation = match observation.state {
                    PeerStageState::Accepted
                    | PeerStageState::InProgress
                    | PeerStageState::Completed => TargetRetirementObservation::InProgress,
                    PeerStageState::Stale => TargetRetirementObservation::Stale,
                    PeerStageState::Failed
                    | PeerStageState::Rejected
                    | PeerStageState::Conflict => TargetRetirementObservation::Failed,
                };
                state.progress.target_retirement = last_observation;
                state.progress.error = observation.error.as_deref().map(normalize_remove_error);
                publish_remove_progress(execution, intent, state, authority_rx, coordinator_tx)
                    .await?;
            }
            Ok(Err(error)) => {
                last_observation = retirement_observation_from_transport_error(&error);
                state.progress.target_retirement = last_observation;
                state.progress.error = Some(error.message);
                publish_remove_progress(execution, intent, state, authority_rx, coordinator_tx)
                    .await?;
            }
            Err(()) => {
                last_observation = TargetRetirementObservation::Unavailable;
                state.progress.target_retirement = last_observation;
                state.progress.error = Some("target Retire call timed out".to_string());
                publish_remove_progress(execution, intent, state, authority_rx, coordinator_tx)
                    .await?;
            }
        }
        wait_for_remove_observation_only(authority_rx).await?;
    }
}

fn validate_remove_peer_status(
    intent: &RemoveReplicaIntent,
    status: &PeerLifecycleStatus,
) -> std::result::Result<(), String> {
    if status.protocol_version != REPLICA_LIFECYCLE_PEER_PROTOCOL_VERSION
        || status.target_replica_id != intent.target_replica_id
        || status.target_instance_id != intent.target_instance_id
        || Some(&status.agent_generation) != intent.expected_target_agent_generation.as_ref()
        || status.epoch != intent.epoch
    {
        return Err("target lifecycle status is stale for the remove intent".to_string());
    }
    Ok(())
}

fn retirement_observation_from_transport_error(
    error: &LifecyclePeerTransportError,
) -> TargetRetirementObservation {
    match error.kind {
        LifecyclePeerTransportErrorKind::Unavailable => TargetRetirementObservation::Unavailable,
        LifecyclePeerTransportErrorKind::Stale => TargetRetirementObservation::Stale,
    }
}

fn remove_connection_absent(
    runtime_status_rx: &watch::Receiver<RuntimeControlSnapshot>,
    intent: &RemoveReplicaIntent,
) -> bool {
    runtime_status_rx
        .borrow()
        .partition_state
        .as_deref()
        .is_some_and(|state| {
            !state.active_replica_connections().iter().any(|connection| {
                connection.id == intent.target_replica_id
                    && connection.instance_id == intent.target_instance_id
            })
        })
}

fn classify_remove_configuration(
    runtime: &RuntimeControlSnapshot,
    intent: &RemoveReplicaIntent,
) -> std::result::Result<RemoveConfigurationObservation, String> {
    let configuration = runtime
        .configuration
        .as_ref()
        .ok_or_else(|| "primary runtime does not expose a configuration".to_string())?;
    if configuration == &intent.previous_status() {
        Ok(RemoveConfigurationObservation::PreviousCurrent)
    } else if configuration == &intent.reduced_catch_up_status() {
        Ok(RemoveConfigurationObservation::ReducedCatchUp)
    } else if configuration == &intent.reduced_current_status() {
        Ok(RemoveConfigurationObservation::ReducedCurrent)
    } else {
        Err("primary runtime configuration is outside the frozen remove intent".to_string())
    }
}

async fn remove_authority_gate(
    execution: &CoordinatorExecution,
    intent: &RemoveReplicaIntent,
    authority_rx: &mut watch::Receiver<Option<CoordinatorAuthority>>,
    runtime_status_rx: &watch::Receiver<RuntimeControlSnapshot>,
    remove_clock: &dyn RemoveReplicaClock,
    deadline: Option<i64>,
    allowed_configurations: &[RemoveConfigurationObservation],
) -> std::result::Result<RemoveConfigurationObservation, String> {
    wait_for_remove_authority(execution, intent, authority_rx).await?;
    if deadline.is_some_and(|deadline| remove_clock.unix_seconds() >= deadline) {
        return Err("remove-replica coordinator deadline expired".to_string());
    }
    let runtime = runtime_status_rx.borrow();
    if runtime.instance_id != intent.primary_instance_id
        || runtime.role != Role::Primary
        || runtime.epoch != intent.epoch
        || runtime.partition_state.is_none()
        || runtime
            .partition_state
            .as_deref()
            .is_none_or(|state| state.write_status() != AccessStatus::Granted)
    {
        return Err(
            "remove-replica coordinator lost exact live-primary write authority".to_string(),
        );
    }
    let configuration = classify_remove_configuration(&runtime, intent)?;
    if !allowed_configurations.contains(&configuration) {
        return Err(format!(
            "remove-replica coordinator phase does not allow configuration {configuration:?}"
        ));
    }
    Ok(configuration)
}

async fn wait_for_remove_authority(
    execution: &CoordinatorExecution,
    intent: &RemoveReplicaIntent,
    authority_rx: &mut watch::Receiver<Option<CoordinatorAuthority>>,
) -> std::result::Result<(), String> {
    loop {
        let expected = execution.current_id();
        let authority = authority_rx.borrow().clone();
        match authority {
            Some(authority)
                if authority.generation == expected.generation
                    && authority.action_id == expected.action_id
                    && authority.action_signature == intent.input_signature
                    && authority.attempt_id == expected.attempt_id
                    && authority.phase_sequence == expected.phase_sequence =>
            {
                return Ok(());
            }
            Some(authority)
                if authority.generation == expected.generation
                    && authority.action_id == expected.action_id
                    && authority.action_signature == intent.input_signature
                    && authority.attempt_id == expected.attempt_id
                    && authority.phase_sequence < expected.phase_sequence =>
            {
                if authority_rx.changed().await.is_err() {
                    return Err("remove-replica parent authority closed".to_string());
                }
            }
            _ => {
                return Err(
                    "remove-replica coordinator generation/action/attempt/phase authority is stale"
                        .to_string(),
                );
            }
        }
    }
}

async fn transition_remove_progress(
    execution: &CoordinatorExecution,
    intent: &RemoveReplicaIntent,
    state: &mut RemoveCoordinatorState,
    authority_rx: &mut watch::Receiver<Option<CoordinatorAuthority>>,
    coordinator_tx: &mpsc::UnboundedSender<CoordinatorEvent>,
    phase: RemoveReplicaCoordinatorPhase,
) -> std::result::Result<(), String> {
    state.progress.phase = phase;
    publish_remove_progress(execution, intent, state, authority_rx, coordinator_tx).await?;
    info!(
        action_id = execution.action_id,
        attempt_id = execution.attempt_id,
        ?phase,
        "remove-replica coordinator phase"
    );
    Ok(())
}

async fn publish_remove_progress(
    execution: &CoordinatorExecution,
    intent: &RemoveReplicaIntent,
    state: &RemoveCoordinatorState,
    authority_rx: &mut watch::Receiver<Option<CoordinatorAuthority>>,
    coordinator_tx: &mpsc::UnboundedSender<CoordinatorEvent>,
) -> std::result::Result<(), String> {
    intent.validate_progress(&state.progress)?;
    let execution_id = execution.advance();
    debug!(
        action_id = execution_id.action_id,
        attempt_id = execution_id.attempt_id,
        phase_sequence = execution_id.phase_sequence,
        phase = ?state.progress.phase,
        commit_observed = state.progress.commit_observed,
        connection_absent = state.progress.connection_absent,
        retirement = ?state.progress.target_retirement,
        "remove-replica coordinator progress"
    );
    coordinator_tx
        .send(CoordinatorEvent {
            execution_id,
            update: CoordinatorUpdate::RemoveProgress(state.progress.clone()),
        })
        .map_err(|_| "remove-replica coordinator status channel closed".to_string())?;
    wait_for_remove_authority(execution, intent, authority_rx).await
}

async fn execute_remove_runtime_effect(
    runtime_tx: &mpsc::Sender<RuntimeEffectCommand>,
    effect: RuntimeEffect,
    remove_clock: &dyn RemoveReplicaClock,
    applicable_deadline: i64,
    call_timeout_seconds: i64,
    operation: &str,
) -> std::result::Result<(), String> {
    let call_deadline =
        remove_call_deadline(remove_clock, applicable_deadline, call_timeout_seconds)?;
    match await_remove_clock_bounded(
        execute_runtime(runtime_tx, effect),
        remove_clock,
        call_deadline,
    )
    .await
    {
        Ok(Ok(RuntimeEffectResult::Unit)) => Ok(()),
        Ok(Ok(RuntimeEffectResult::DataLoss(_))) => Err(format!(
            "remove-replica {operation} returned an unexpected data-loss result"
        )),
        Ok(Err(error)) => Err(normalize_remove_error(&error.to_string())),
        Err(()) => Err(format!("remove-replica {operation} timed out")),
    }
}

fn remove_call_deadline(
    remove_clock: &dyn RemoveReplicaClock,
    applicable_deadline: i64,
    call_timeout_seconds: i64,
) -> std::result::Result<i64, String> {
    let now = remove_clock.unix_seconds();
    if now >= applicable_deadline {
        return Err("remove-replica applicable deadline expired".to_string());
    }
    Ok(now
        .saturating_add(call_timeout_seconds)
        .min(applicable_deadline))
}

fn remove_timeout_duration(
    remove_clock: &dyn RemoveReplicaClock,
    deadline: i64,
) -> std::result::Result<std::time::Duration, String> {
    let remaining = deadline.saturating_sub(remove_clock.unix_seconds());
    if remaining <= 0 {
        return Err("remove-replica call deadline expired".to_string());
    }
    Ok(std::time::Duration::from_secs(remaining as u64))
}

async fn await_remove_clock_bounded<F, T>(
    future: F,
    remove_clock: &dyn RemoveReplicaClock,
    deadline: i64,
) -> std::result::Result<T, ()>
where
    F: std::future::Future<Output = T>,
{
    tokio::pin!(future);
    loop {
        if remove_clock.unix_seconds() >= deadline {
            return Err(());
        }
        tokio::select! {
            result = &mut future => return Ok(result),
            _ = tokio::time::sleep(std::time::Duration::from_millis(10)) => {}
        }
    }
}

async fn wait_for_remove_observation(
    runtime_status_rx: &mut watch::Receiver<RuntimeControlSnapshot>,
    authority_rx: &mut watch::Receiver<Option<CoordinatorAuthority>>,
) -> std::result::Result<(), String> {
    tokio::select! {
        changed = runtime_status_rx.changed() => {
            changed.map_err(|_| "remove-replica runtime observation channel closed".to_string())
        }
        changed = authority_rx.changed() => {
            changed.map_err(|_| "remove-replica parent authority channel closed".to_string())
        }
        _ = tokio::time::sleep(std::time::Duration::from_millis(10)) => Ok(()),
    }
}

async fn wait_for_remove_observation_only(
    authority_rx: &mut watch::Receiver<Option<CoordinatorAuthority>>,
) -> std::result::Result<(), String> {
    tokio::select! {
        changed = authority_rx.changed() => {
            changed.map_err(|_| "remove-replica parent authority channel closed".to_string())
        }
        _ = tokio::time::sleep(std::time::Duration::from_millis(10)) => Ok(()),
    }
}

async fn run_add_replica_coordinator(
    execution: CoordinatorExecution,
    intent: crate::add_replica::AddReplicaIntent,
    parent_action_id: String,
    parent_action_signature: String,
    runtime_tx: mpsc::Sender<RuntimeEffectCommand>,
    mut runtime_status_rx: watch::Receiver<RuntimeControlSnapshot>,
    coordinator_tx: mpsc::UnboundedSender<CoordinatorEvent>,
) {
    let result = run_add_replica_coordinator_inner(
        &execution,
        &intent,
        &parent_action_id,
        &parent_action_signature,
        &runtime_tx,
        &mut runtime_status_rx,
        &coordinator_tx,
    )
    .await;
    let terminal = match result {
        Ok(result) => Ok(result),
        Err(error) => {
            warn!(error = %error, "add-replica coordinator entering compensation");
            let commit_observed = runtime_status_rx.borrow().configuration.as_ref()
                == Some(
                    &intent
                        .current_configuration
                        .status(crate::types::ReplicaConfigurationMode::Current),
                );
            if commit_observed {
                Err(error)
            } else {
                match compensate_add_replica(
                    &execution,
                    &intent,
                    &parent_action_id,
                    &parent_action_signature,
                    &runtime_tx,
                    &mut runtime_status_rx,
                    &coordinator_tx,
                )
                .await
                {
                    Ok(()) => Ok(AddReplicaTerminalResult::Compensated),
                    Err(_compensation_error) => {
                        let _ = coordinator_tx.send(CoordinatorEvent {
                            execution_id: execution.advance(),
                            update: CoordinatorUpdate::AddProgress(AddReplicaProgress {
                                phase: AddReplicaCoordinatorPhase::Compensating,
                                commit_observed: false,
                                copy_lsn: None,
                            }),
                        });
                        Ok(AddReplicaTerminalResult::CompensationIncomplete)
                    }
                }
            }
        }
    };
    let _ = coordinator_tx.send(CoordinatorEvent {
        execution_id: execution.current_id(),
        update: CoordinatorUpdate::AddTerminal(terminal),
    });
}

async fn run_add_replica_coordinator_inner(
    execution: &CoordinatorExecution,
    intent: &crate::add_replica::AddReplicaIntent,
    parent_action_id: &str,
    parent_action_signature: &str,
    runtime_tx: &mpsc::Sender<RuntimeEffectCommand>,
    runtime_status_rx: &mut watch::Receiver<RuntimeControlSnapshot>,
    coordinator_tx: &mpsc::UnboundedSender<CoordinatorEvent>,
) -> std::result::Result<AddReplicaTerminalResult, String> {
    ensure_add_deadline(intent.deadline_unix_seconds)?;
    let current_status = intent
        .current_configuration
        .status(crate::types::ReplicaConfigurationMode::Current);
    if runtime_status_rx.borrow().configuration.as_ref() == Some(&current_status) {
        send_add_progress(
            coordinator_tx,
            execution,
            AddReplicaCoordinatorPhase::Attesting,
            true,
            runtime_status_rx
                .borrow()
                .build_observation
                .as_ref()
                .and_then(|build| build.copy_lsn),
        );
        return attest_add_replica(
            intent,
            parent_action_id,
            parent_action_signature,
            runtime_status_rx,
        )
        .await
        .map(|_| AddReplicaTerminalResult::Committed);
    }

    if let Some(retired) = &intent.retired_instance_id
        && runtime_status_rx
            .borrow()
            .partition_state
            .as_deref()
            .is_some_and(|state| {
                state.active_replica_connections().iter().any(|connection| {
                    connection.id == intent.target_replica_id && &connection.instance_id == retired
                })
            })
    {
        send_add_progress(
            coordinator_tx,
            execution,
            AddReplicaCoordinatorPhase::RetiringOldConnection,
            false,
            None,
        );
        execute_runtime(
            runtime_tx,
            RuntimeEffect::RemoveReplica {
                replica_id: intent.target_replica_id,
                instance_id: retired.clone(),
            },
        )
        .await
        .map_err(|error| error.to_string())?;
    }

    send_add_progress(
        coordinator_tx,
        execution,
        AddReplicaCoordinatorPhase::PreparingTarget,
        false,
        None,
    );
    ensure_peer_stage(
        intent,
        parent_action_id,
        parent_action_signature,
        PeerStage::Prepare,
        None,
        None,
    )
    .await?;

    send_add_progress(
        coordinator_tx,
        execution,
        AddReplicaCoordinatorPhase::Building,
        false,
        None,
    );
    let build_key = intent.semantic_build_key();
    let existing_copy_lsn = {
        let runtime = runtime_status_rx.borrow();
        exact_completed_build(runtime.build_observation.as_ref(), intent, &build_key)
    };
    let copy_lsn = match existing_copy_lsn {
        Some(copy_lsn) => copy_lsn,
        None => {
            let build_execution_id = format!("{}:build", intent.attempt_id);
            execute_runtime(
                runtime_tx,
                RuntimeEffect::StartTrackedBuild {
                    execution_id: build_execution_id.clone(),
                    build_key: build_key.clone(),
                    target_agent_generation: intent.target_agent_generation.clone(),
                    replica: ReplicaInfo {
                        id: intent.target_replica_id,
                        instance_id: intent.target_instance_id.clone(),
                        role: crate::types::Role::IdleSecondary,
                        status: crate::types::ReplicaStatus::Up,
                        replicator_address: intent.target_replicator_address.clone(),
                        current_progress: 0,
                        catch_up_capability: 0,
                        must_catch_up: false,
                    },
                },
            )
            .await
            .map_err(|error| error.to_string())?;
            wait_for_build(
                runtime_tx,
                runtime_status_rx,
                intent,
                &build_key,
                &build_execution_id,
            )
            .await?
        }
    };

    send_add_progress(
        coordinator_tx,
        execution,
        AddReplicaCoordinatorPhase::ActivatingTarget,
        false,
        Some(copy_lsn),
    );
    ensure_peer_stage(
        intent,
        parent_action_id,
        parent_action_signature,
        PeerStage::Activate,
        Some(build_key.clone()),
        Some(copy_lsn),
    )
    .await?;

    let catch_up_status = intent
        .catch_up_configuration
        .status(crate::types::ReplicaConfigurationMode::CatchUp);
    if runtime_status_rx.borrow().configuration.as_ref() != Some(&catch_up_status)
        && runtime_status_rx.borrow().configuration.as_ref() != Some(&current_status)
    {
        send_add_progress(
            coordinator_tx,
            execution,
            AddReplicaCoordinatorPhase::InstallingCatchUpConfiguration,
            false,
            Some(copy_lsn),
        );
        execute_runtime(
            runtime_tx,
            RuntimeEffect::UpdateTrackedCatchUpConfiguration {
                current: intent.catch_up_configuration.materialize(Some(copy_lsn))?,
                previous: intent.previous_configuration.materialize(None)?,
                required_build_key: build_key.clone(),
            },
        )
        .await
        .map_err(|error| error.to_string())?;
    }

    if runtime_status_rx.borrow().configuration.as_ref() != Some(&current_status) {
        send_add_progress(
            coordinator_tx,
            execution,
            AddReplicaCoordinatorPhase::WaitingForCatchUpQuorum,
            false,
            Some(copy_lsn),
        );
        let wait_id = format!("{}:wait", intent.attempt_id);
        execute_runtime(
            runtime_tx,
            RuntimeEffect::StartTrackedCatchUpQuorum {
                execution_id: wait_id.clone(),
                mode: crate::types::ReplicaSetQuorumMode::Write,
            },
        )
        .await
        .map_err(|error| error.to_string())?;
        wait_for_quorum(runtime_tx, runtime_status_rx, intent, &wait_id).await?;

        send_add_progress(
            coordinator_tx,
            execution,
            AddReplicaCoordinatorPhase::InstallingCurrentConfiguration,
            false,
            Some(copy_lsn),
        );
        execute_runtime(
            runtime_tx,
            RuntimeEffect::UpdateCurrentConfiguration {
                current: intent.current_configuration.materialize(Some(copy_lsn))?,
            },
        )
        .await
        .map_err(|error| error.to_string())?;
    }

    send_add_progress(
        coordinator_tx,
        execution,
        AddReplicaCoordinatorPhase::Attesting,
        true,
        Some(copy_lsn),
    );
    attest_add_replica(
        intent,
        parent_action_id,
        parent_action_signature,
        runtime_status_rx,
    )
    .await?;
    Ok(AddReplicaTerminalResult::Committed)
}

fn exact_completed_build(
    observation: Option<&crate::add_replica::RuntimeBuildObservation>,
    intent: &crate::add_replica::AddReplicaIntent,
    build_key: &str,
) -> Option<Lsn> {
    observation
        .filter(|observation| {
            observation.build_key == build_key
                && observation.target_replica_id == intent.target_replica_id
                && observation.target_instance_id == intent.target_instance_id
                && observation.target_agent_generation == intent.target_agent_generation
                && observation.state == crate::add_replica::RuntimeBuildState::Completed
        })
        .and_then(|observation| observation.copy_lsn)
}

async fn wait_for_build(
    runtime_tx: &mpsc::Sender<RuntimeEffectCommand>,
    runtime_status_rx: &mut watch::Receiver<RuntimeControlSnapshot>,
    intent: &crate::add_replica::AddReplicaIntent,
    build_key: &str,
    execution_id: &str,
) -> std::result::Result<Lsn, String> {
    loop {
        if let Some(copy_lsn) = exact_completed_build(
            runtime_status_rx.borrow().build_observation.as_ref(),
            intent,
            build_key,
        ) {
            return Ok(copy_lsn);
        }
        if let Some(observation) = runtime_status_rx.borrow().build_observation.as_ref()
            && observation.execution_id == execution_id
            && matches!(
                observation.state,
                crate::add_replica::RuntimeBuildState::Failed
                    | crate::add_replica::RuntimeBuildState::Cancelled
            )
        {
            return Err(observation
                .error
                .clone()
                .unwrap_or_else(|| "tracked build failed".to_string()));
        }
        if unix_seconds() >= intent.deadline_unix_seconds {
            let _ = execute_runtime(
                runtime_tx,
                RuntimeEffect::CancelTrackedOperation {
                    execution_id: execution_id.to_string(),
                },
            )
            .await;
            return Err("tracked build reached its deadline".to_string());
        }
        tokio::select! {
            _ = runtime_status_rx.changed() => {}
            _ = tokio::time::sleep(std::time::Duration::from_millis(100)) => {}
        }
    }
}

async fn wait_for_quorum(
    runtime_tx: &mpsc::Sender<RuntimeEffectCommand>,
    runtime_status_rx: &mut watch::Receiver<RuntimeControlSnapshot>,
    intent: &crate::add_replica::AddReplicaIntent,
    execution_id: &str,
) -> std::result::Result<(), String> {
    loop {
        if let Some(observation) = runtime_status_rx.borrow().quorum_wait_observation.as_ref()
            && observation.execution_id == execution_id
        {
            match observation.state {
                crate::add_replica::RuntimeBuildState::Completed => return Ok(()),
                crate::add_replica::RuntimeBuildState::Failed
                | crate::add_replica::RuntimeBuildState::Cancelled => {
                    return Err(observation
                        .error
                        .clone()
                        .unwrap_or_else(|| "tracked quorum wait failed".to_string()));
                }
                crate::add_replica::RuntimeBuildState::InProgress => {}
            }
        }
        if unix_seconds() >= intent.deadline_unix_seconds {
            let _ = execute_runtime(
                runtime_tx,
                RuntimeEffect::CancelTrackedOperation {
                    execution_id: execution_id.to_string(),
                },
            )
            .await;
            return Err("tracked quorum wait reached its deadline".to_string());
        }
        tokio::select! {
            _ = runtime_status_rx.changed() => {}
            _ = tokio::time::sleep(std::time::Duration::from_millis(100)) => {}
        }
    }
}

async fn ensure_peer_stage(
    intent: &crate::add_replica::AddReplicaIntent,
    parent_action_id: &str,
    parent_action_signature: &str,
    stage: PeerStage,
    build_key: Option<String>,
    copy_lsn: Option<Lsn>,
) -> std::result::Result<(), String> {
    let client = crate::grpc::peer_client::GrpcPeerClient::connect(
        intent.target_control_address.clone(),
        intent.target_replica_id,
        intent.target_instance_id.clone(),
        intent.target_agent_generation.clone(),
    )
    .await
    .map_err(|error| error.to_string())?;
    loop {
        ensure_add_deadline(intent.deadline_unix_seconds)?;
        let timeout = remaining_peer_timeout(intent.deadline_unix_seconds)?;
        let status = client
            .get_status(timeout)
            .await
            .map_err(|error| error.to_string())?;
        let postcondition = match stage {
            PeerStage::Prepare => {
                status.epoch == intent.epoch
                    && matches!(
                        status.role,
                        crate::types::Role::IdleSecondary | crate::types::Role::ActiveSecondary
                    )
            }
            PeerStage::Activate => {
                status.epoch == intent.epoch
                    && status.role == crate::types::Role::ActiveSecondary
                    && status.current_progress >= copy_lsn.unwrap_or_default()
            }
            PeerStage::Cleanup => !status.healthy,
            PeerStage::Retire => {
                return Err("add-replica coordinator cannot issue Retire".to_string());
            }
        };
        if postcondition {
            return Ok(());
        }
        let message_id = format!("{}:{stage:?}", intent.attempt_id);
        if let Some(observation) = status
            .current_action
            .as_ref()
            .into_iter()
            .chain(status.retained_terminal_actions.iter().rev())
            .find(|observation| observation.message_id == message_id)
        {
            match observation.state {
                PeerStageState::Completed => {
                    return Err(
                        "completed peer stage no longer satisfies its live postcondition"
                            .to_string(),
                    );
                }
                PeerStageState::Failed
                | PeerStageState::Stale
                | PeerStageState::Rejected
                | PeerStageState::Conflict => {
                    return Err(observation
                        .error
                        .clone()
                        .unwrap_or_else(|| "peer stage failed".to_string()));
                }
                PeerStageState::Accepted | PeerStageState::InProgress => {
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    continue;
                }
            }
        }
        let mut request = PeerStageRequest {
            protocol_version: REPLICA_LIFECYCLE_PEER_PROTOCOL_VERSION,
            operation_kind: PeerOperationKind::AddBuild,
            stage_semantic_version: PEER_STAGE_SEMANTIC_VERSION,
            operation_id: intent.operation_id.clone(),
            attempt_id: intent.attempt_id.clone(),
            message_id,
            input_signature: String::new(),
            stage,
            sender_replica_id: intent.primary_replica_id,
            sender_instance_id: intent.primary_instance_id.clone(),
            sender_agent_generation: intent.primary_agent_generation.clone(),
            sender_control_address: intent.primary_control_address.clone(),
            parent_action_id: parent_action_id.to_string(),
            parent_action_signature: parent_action_signature.to_string(),
            target_replica_id: intent.target_replica_id,
            target_instance_id: intent.target_instance_id.clone(),
            expected_target_agent_generation: intent.target_agent_generation.clone(),
            expected_target_peer_control_version: status.peer_control_version,
            epoch: intent.epoch,
            configuration_fence: intent.configuration_fence(),
            build_key: build_key.clone(),
            copy_lsn,
            removal_mode: None,
            commit_observed_unix_seconds: None,
            retirement_expiry_unix_seconds: None,
            reduced_current_projection: None,
        };
        request.input_signature = request.signature();
        let observation = client
            .execute_stage(
                request,
                remaining_peer_timeout(intent.deadline_unix_seconds)?,
            )
            .await
            .map_err(|error| error.to_string())?;
        match observation.state {
            PeerStageState::Completed => return Ok(()),
            PeerStageState::Failed
            | PeerStageState::Stale
            | PeerStageState::Rejected
            | PeerStageState::Conflict => {
                return Err(observation
                    .error
                    .unwrap_or_else(|| "peer stage failed".to_string()));
            }
            PeerStageState::Accepted | PeerStageState::InProgress => {
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        }
    }
}

async fn attest_add_replica(
    intent: &crate::add_replica::AddReplicaIntent,
    parent_action_id: &str,
    parent_action_signature: &str,
    runtime_status_rx: &watch::Receiver<RuntimeControlSnapshot>,
) -> std::result::Result<(), String> {
    let runtime = runtime_status_rx.borrow().clone();
    if runtime.role != crate::types::Role::Primary
        || runtime.epoch != intent.epoch
        || runtime.configuration.as_ref()
            != Some(
                &intent
                    .current_configuration
                    .status(crate::types::ReplicaConfigurationMode::Current),
            )
        || !runtime.partition_state.as_deref().is_some_and(|state| {
            state.active_replica_connections().iter().any(|connection| {
                connection.id == intent.target_replica_id
                    && connection.instance_id == intent.target_instance_id
            })
        })
    {
        return Err("primary add-replica final attestation failed".to_string());
    }
    drop(runtime);
    let copy_lsn = runtime_status_rx
        .borrow()
        .build_observation
        .as_ref()
        .and_then(|build| exact_completed_build(Some(build), intent, &intent.semantic_build_key()));
    ensure_peer_stage(
        intent,
        parent_action_id,
        parent_action_signature,
        PeerStage::Activate,
        Some(intent.semantic_build_key()),
        copy_lsn,
    )
    .await
}

async fn compensate_add_replica(
    execution: &CoordinatorExecution,
    intent: &crate::add_replica::AddReplicaIntent,
    parent_action_id: &str,
    parent_action_signature: &str,
    runtime_tx: &mpsc::Sender<RuntimeEffectCommand>,
    runtime_status_rx: &mut watch::Receiver<RuntimeControlSnapshot>,
    coordinator_tx: &mpsc::UnboundedSender<CoordinatorEvent>,
) -> std::result::Result<(), String> {
    send_add_progress(
        coordinator_tx,
        execution,
        AddReplicaCoordinatorPhase::Compensating,
        false,
        None,
    );
    let active_build = {
        let runtime = runtime_status_rx.borrow();
        runtime
            .build_observation
            .as_ref()
            .filter(|build| build.state == crate::add_replica::RuntimeBuildState::InProgress)
            .map(|build| build.execution_id.clone())
    };
    if let Some(execution_id) = active_build {
        let _ = execute_runtime(
            runtime_tx,
            RuntimeEffect::CancelTrackedOperation { execution_id },
        )
        .await;
    }
    let active_wait = {
        let runtime = runtime_status_rx.borrow();
        runtime
            .quorum_wait_observation
            .as_ref()
            .filter(|wait| wait.state == crate::add_replica::RuntimeBuildState::InProgress)
            .map(|wait| wait.execution_id.clone())
    };
    if let Some(execution_id) = active_wait {
        let _ = execute_runtime(
            runtime_tx,
            RuntimeEffect::CancelTrackedOperation { execution_id },
        )
        .await;
    }
    wait_for_tracked_settlement(runtime_status_rx, intent.compensation_deadline_unix_seconds)
        .await?;
    let previous_status = intent
        .previous_configuration
        .status(crate::types::ReplicaConfigurationMode::Current);
    if runtime_status_rx.borrow().configuration.as_ref() != Some(&previous_status) {
        execute_runtime(
            runtime_tx,
            RuntimeEffect::UpdateCurrentConfiguration {
                current: intent.previous_configuration.materialize(None)?,
            },
        )
        .await
        .map_err(|error| error.to_string())?;
    }

    async fn wait_for_tracked_settlement(
        runtime_status_rx: &mut watch::Receiver<RuntimeControlSnapshot>,
        deadline: i64,
    ) -> std::result::Result<(), String> {
        loop {
            let settled = {
                let runtime = runtime_status_rx.borrow();
                let build_settled = runtime.build_observation.as_ref().is_none_or(|build| {
                    build.state != crate::add_replica::RuntimeBuildState::InProgress
                });
                let wait_settled = runtime.quorum_wait_observation.as_ref().is_none_or(|wait| {
                    wait.state != crate::add_replica::RuntimeBuildState::InProgress
                });
                build_settled && wait_settled
            };
            if settled {
                return Ok(());
            }
            if unix_seconds() >= deadline {
                return Err(
                    "tracked runtime operation did not settle before compensation deadline"
                        .to_string(),
                );
            }
            tokio::select! {
                _ = runtime_status_rx.changed() => {}
                _ = tokio::time::sleep(std::time::Duration::from_millis(100)) => {}
            }
        }
    }
    execute_runtime(
        runtime_tx,
        RuntimeEffect::RemoveReplica {
            replica_id: intent.target_replica_id,
            instance_id: intent.target_instance_id.clone(),
        },
    )
    .await
    .map_err(|error| error.to_string())?;
    let _ = ensure_peer_stage(
        intent,
        parent_action_id,
        parent_action_signature,
        PeerStage::Cleanup,
        None,
        None,
    )
    .await;
    if unix_seconds() > intent.compensation_deadline_unix_seconds {
        return Err("add-replica compensation reached its deadline".to_string());
    }
    let runtime = runtime_status_rx.borrow();
    let configuration_restored = runtime.configuration.as_ref() == Some(&previous_status);
    let connection_absent = runtime.partition_state.as_deref().is_none_or(|state| {
        !state.active_replica_connections().iter().any(|connection| {
            connection.id == intent.target_replica_id
                && connection.instance_id == intent.target_instance_id
        })
    });
    if configuration_restored && connection_absent {
        Ok(())
    } else {
        Err("add-replica compensation barrier is not proven".to_string())
    }
}

async fn execute_runtime(
    runtime_tx: &mpsc::Sender<RuntimeEffectCommand>,
    effect: RuntimeEffect,
) -> Result<RuntimeEffectResult> {
    let (reply_tx, reply_rx) = oneshot::channel();
    runtime_tx
        .send(RuntimeEffectCommand {
            effect,
            reply: reply_tx,
        })
        .await
        .map_err(|_| KubericError::Closed)?;
    reply_rx.await.unwrap_or(Err(KubericError::Closed))
}

fn send_add_progress(
    coordinator_tx: &mpsc::UnboundedSender<CoordinatorEvent>,
    execution: &CoordinatorExecution,
    phase: AddReplicaCoordinatorPhase,
    commit_observed: bool,
    copy_lsn: Option<Lsn>,
) {
    info!(
        ?phase,
        commit_observed,
        ?copy_lsn,
        "add-replica coordinator phase"
    );
    let _ = coordinator_tx.send(CoordinatorEvent {
        execution_id: execution.advance(),
        update: CoordinatorUpdate::AddProgress(AddReplicaProgress {
            phase,
            commit_observed,
            copy_lsn,
        }),
    });
}

fn ensure_add_deadline(deadline: i64) -> std::result::Result<(), String> {
    if unix_seconds() >= deadline {
        Err("add-replica intent reached its deadline".to_string())
    } else {
        Ok(())
    }
}

fn remaining_peer_timeout(deadline: i64) -> std::result::Result<std::time::Duration, String> {
    let remaining = deadline.saturating_sub(unix_seconds());
    if remaining <= 0 {
        return Err("add-replica intent reached its deadline".to_string());
    }
    Ok(std::time::Duration::from_secs(remaining as u64).min(std::time::Duration::from_secs(5)))
}

fn unix_seconds() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs() as i64)
}

fn correlated_runtime_effect(action: DurableReplicaAction) -> RuntimeEffect {
    match action {
        DurableReplicaAction::AddReplicaIntent { .. } => {
            unreachable!("add-replica intent is handled by the coordinator")
        }
        DurableReplicaAction::RemoveReplicaIntent { .. } => {
            unreachable!("remove-replica intent is handled by the coordinator")
        }
        DurableReplicaAction::Open { mode } => RuntimeEffect::Open { mode },
        DurableReplicaAction::Close => RuntimeEffect::Close,
        DurableReplicaAction::RevokeWriteStatus => RuntimeEffect::RevokeWriteStatus,
        DurableReplicaAction::ChangeRole { epoch, role } => {
            RuntimeEffect::ChangeRole { epoch, role }
        }
        DurableReplicaAction::UpdateEpoch { epoch } => RuntimeEffect::UpdateEpoch { epoch },
        DurableReplicaAction::UpdateCatchUpConfiguration { current, previous } => {
            RuntimeEffect::UpdateCatchUpConfiguration { current, previous }
        }
        DurableReplicaAction::WaitForCatchUpQuorum { mode } => {
            RuntimeEffect::WaitForCatchUpQuorum { mode }
        }
        DurableReplicaAction::UpdateCurrentConfiguration { current } => {
            RuntimeEffect::UpdateCurrentConfiguration { current }
        }
        DurableReplicaAction::BuildReplica { replica } => RuntimeEffect::BuildReplica { replica },
        DurableReplicaAction::RemoveReplica {
            replica_id,
            instance_id,
        } => RuntimeEffect::RemoveReplica {
            replica_id,
            instance_id,
        },
        DurableReplicaAction::OnDataLoss { epoch } => RuntimeEffect::OnDataLoss {
            expected_epoch: epoch,
        },
        DurableReplicaAction::RecordElectionConfiguration { configuration } => {
            RuntimeEffect::RecordElectionConfiguration { configuration }
        }
    }
}

fn reject_agent_command(command: AgentCommand, error: KubericError) {
    match command {
        AgentCommand::ExecuteCorrelatedControlAction { reply, .. } => {
            let _ = reply.send(Err(error));
        }
        AgentCommand::ExecuteLifecycleStage { reply, .. } => {
            let _ = reply.send(Err(error));
        }
        AgentCommand::GetLifecycleStatus { reply, .. } => {
            let _ = reply.send(Err(error));
        }
        AgentCommand::GetStatus { .. } => {}
    }
}

fn send_observation(
    reply: oneshot::Sender<Result<CorrelatedControlActionAcknowledgement>>,
    observation: CorrelatedActionObservation,
) {
    let _ = reply.send(Ok(CorrelatedControlActionAcknowledgement { observation }));
}

fn normalize_error(error: &str) -> String {
    if error.len() <= MAX_ERROR_BYTES {
        return error.to_string();
    }

    let mut boundary = MAX_ERROR_BYTES;
    while !error.is_char_boundary(boundary) {
        boundary -= 1;
    }
    error[..boundary].to_string()
}

fn terminal_error_class(error: &KubericError) -> DurableActionErrorClass {
    match error {
        KubericError::NotPrimary => DurableActionErrorClass::NotPrimary,
        KubericError::NoWriteQuorum => DurableActionErrorClass::NoWriteQuorum,
        KubericError::ReconfigurationPending => DurableActionErrorClass::ReconfigurationPending,
        KubericError::StaleEpoch { .. } => DurableActionErrorClass::StaleEpoch,
        KubericError::Cancelled => DurableActionErrorClass::Cancelled,
        KubericError::Closed => DurableActionErrorClass::Closed,
        _ => DurableActionErrorClass::Internal,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::time::Duration;
    use tonic::transport::Server;

    use crate::grpc::peer_server::PeerServer;
    use crate::grpc::server::ControlServer;
    use crate::handles::PartitionState;
    use crate::proto::replica_lifecycle_peer_server::ReplicaLifecyclePeerServer;
    use crate::proto::replicator_control_server::ReplicatorControlServer;
    use crate::remove_replica::{
        MAX_REMOVE_REPLICA_PRE_COMMIT_ATTEMPTS, ManualRemoveReplicaClock,
        REMOVE_REPLICA_COMPENSATION_GRACE_SECONDS, REMOVE_REPLICA_RETIREMENT_TIMEOUT_SECONDS,
        RemoveReplicaMode,
    };
    use crate::replica_lifecycle::{
        ConfigurationDescriptor, ConfigurationMemberDescriptor, ConfigurationProgressSource,
        RETIRE_STAGE_SEMANTIC_VERSION,
    };
    use crate::types::{
        Epoch, FaultType, ReplicaConfigurationMemberStatus, ReplicaConfigurationMode,
        ReplicaConfigurationStatus, ReplicaStatus, Role,
    };

    fn test_agent(
        epoch: Epoch,
        opened: bool,
    ) -> (ReplicaAgent, mpsc::Receiver<RuntimeEffectCommand>) {
        let (_command_tx, command_rx) = mpsc::channel(32);
        let (runtime_tx, runtime_rx) = mpsc::channel(32);
        let (_fault_tx, fault_rx) = mpsc::channel(16);
        let (_status_tx, status_rx) = watch::channel(RuntimeControlSnapshot {
            instance_id: ReplicaInstanceId::new("pod-uid"),
            role: if opened { Role::Primary } else { Role::Unknown },
            epoch,
            configuration: None,
            election_configuration: None,
            deactivation_info: None,
            build_observation: None,
            quorum_wait_observation: None,
            partition_state: opened.then(|| Arc::new(PartitionState::new())),
        });
        (
            ReplicaAgent::new(
                1,
                ReplicaInstanceId::new("pod-uid"),
                command_rx,
                runtime_tx,
                status_rx,
                fault_rx,
                CancellationToken::new(),
            ),
            runtime_rx,
        )
    }

    fn terminal(action_id: &str) -> CorrelatedActionObservation {
        CorrelatedActionObservation {
            generation: AgentGeneration::from_string("generation"),
            control_version: AgentControlVersion::new(1),
            action: DurableActionObservation {
                action_id: action_id.to_string(),
                signature: DurableReplicaAction::Close.signature(),
                state: DurableActionState::Completed,
                error_class: None,
                error: None,
                result: None,
                add_replica_progress: None,
                remove_replica_progress: None,
            },
        }
    }

    fn request(
        agent: &ReplicaAgent,
        action_id: &str,
        action: DurableReplicaAction,
    ) -> CorrelatedControlActionRequest {
        CorrelatedControlActionRequest {
            protocol_version: CORRELATED_CONTROL_PROTOCOL_VERSION,
            action_id: action_id.to_string(),
            input_signature: action.signature(),
            target_replica_id: agent.replica_id,
            target_instance_id: agent.instance_id.clone(),
            expected_agent_generation: agent.generation.clone(),
            expected_control_version: agent.control_version,
            observed_runtime_epoch: agent.runtime_status_rx.borrow().epoch,
            action,
        }
    }

    async fn accept(
        agent: &mut ReplicaAgent,
        request: CorrelatedControlActionRequest,
    ) -> Result<CorrelatedControlActionAcknowledgement> {
        let (reply_tx, reply_rx) = oneshot::channel();
        agent.accept_correlated(request, reply_tx);
        reply_rx.await.unwrap()
    }

    fn peer_request(agent: &ReplicaAgent, message_id: &str) -> PeerStageRequest {
        let mut request = PeerStageRequest {
            protocol_version: REPLICA_LIFECYCLE_PEER_PROTOCOL_VERSION,
            operation_kind: PeerOperationKind::AddBuild,
            stage_semantic_version: PEER_STAGE_SEMANTIC_VERSION,
            operation_id: "operation".to_string(),
            attempt_id: "attempt-1".to_string(),
            message_id: message_id.to_string(),
            input_signature: String::new(),
            stage: PeerStage::Prepare,
            sender_replica_id: 9,
            sender_instance_id: ReplicaInstanceId::new("sender"),
            sender_agent_generation: AgentGeneration::parse("11111111111111111111111111111111")
                .unwrap(),
            sender_control_address: "http://127.0.0.1:1".to_string(),
            parent_action_id: "parent".to_string(),
            parent_action_signature: "parent-signature".to_string(),
            target_replica_id: agent.replica_id,
            target_instance_id: agent.instance_id.clone(),
            expected_target_agent_generation: agent.generation.clone(),
            expected_target_peer_control_version: agent.peer_control_version,
            epoch: agent.runtime_status_rx.borrow().epoch,
            configuration_fence: "configuration".to_string(),
            build_key: None,
            copy_lsn: None,
            removal_mode: None,
            commit_observed_unix_seconds: None,
            retirement_expiry_unix_seconds: None,
            reduced_current_projection: None,
        };
        request.input_signature = request.signature();
        request
    }

    async fn accept_peer(
        agent: &mut ReplicaAgent,
        request: PeerStageRequest,
    ) -> Result<PeerStageObservation> {
        let (reply_tx, reply_rx) = oneshot::channel();
        agent.accept_peer_stage(request, reply_tx);
        reply_rx.await.unwrap()
    }

    fn generation(value: char) -> AgentGeneration {
        AgentGeneration::parse(value.to_string().repeat(32)).unwrap()
    }

    fn reduced_current_projection() -> ReplicaConfigurationStatus {
        ReplicaConfigurationStatus {
            mode: ReplicaConfigurationMode::Current,
            members: vec![ReplicaConfigurationMemberStatus {
                id: 3,
                instance_id: ReplicaInstanceId::new("retained"),
                role: Role::ActiveSecondary,
            }],
            write_quorum: 2,
        }
    }

    fn retire_configuration_fence(reduced_projection: &ReplicaConfigurationStatus) -> String {
        let reduced_members = reduced_projection
            .members
            .iter()
            .map(|member| {
                format!(
                    "{}@{}:{:?}:Up:http://replica-{}:7001:false:frozen:{}:{}",
                    member.id,
                    member.instance_id,
                    member.role,
                    member.id,
                    member.id * 10,
                    member.id * 10
                )
            })
            .collect::<Vec<_>>()
            .join(",");
        let reduced = format!("q{}[{reduced_members}]", reduced_projection.write_quorum);
        let previous = "q2[2@target:ActiveSecondary:Up:http://replica-2:7001:false:frozen:20:20,3@retained:ActiveSecondary:Up:http://replica-3:7001:false:frozen:30:30]";
        format!("previous={previous};reduced-catch-up={reduced};reduced-current={reduced}")
    }

    fn retire_parent_signature(reduced_projection: &ReplicaConfigurationStatus) -> String {
        format!(
            "remove-parent:{}:2",
            retire_configuration_fence(reduced_projection)
        )
    }

    fn sender_status(configuration: ReplicaConfigurationStatus) -> ReplicaStatusInfo {
        let parent_signature = retire_parent_signature(&configuration);
        let sender_generation = generation('a');
        ReplicaStatusInfo {
            instance_id: ReplicaInstanceId::new("primary"),
            role: Role::Primary,
            epoch: Epoch::new(4, 9),
            current_progress: 20,
            catch_up_capability: Some(20),
            committed_lsn: 20,
            healthy: true,
            write_status: AccessStatus::Granted,
            configuration: Some(configuration),
            election_configuration: None,
            deactivation_info: None,
            active_replica_connections: Vec::new(),
            build_observation: None,
            agent: ReplicaAgentStatus {
                protocol_version: CORRELATED_CONTROL_PROTOCOL_VERSION,
                lifecycle_peer_protocol_version: REPLICA_LIFECYCLE_PEER_PROTOCOL_VERSION,
                generation: sender_generation.clone(),
                control_version: AgentControlVersion::new(1),
                current_action: Some(CorrelatedActionObservation {
                    generation: sender_generation,
                    control_version: AgentControlVersion::new(1),
                    action: DurableActionObservation {
                        action_id: "remove-parent".to_string(),
                        signature: parent_signature,
                        state: DurableActionState::InProgress,
                        error_class: None,
                        error: None,
                        result: None,
                        add_replica_progress: None,
                        remove_replica_progress: None,
                    },
                }),
                retained_terminal_actions: Vec::new(),
                local_faults: Vec::new(),
            },
        }
    }

    struct SenderStatusServer {
        address: String,
        status_tx: watch::Sender<ReplicaStatusInfo>,
        shutdown: CancellationToken,
    }

    impl SenderStatusServer {
        async fn start(status: ReplicaStatusInfo) -> Self {
            Self::start_with_delay(status, Duration::ZERO).await
        }

        async fn start_with_delay(status: ReplicaStatusInfo, status_delay: Duration) -> Self {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = format!("http://{}", listener.local_addr().unwrap());
            let (command_tx, mut command_rx) = mpsc::channel(16);
            let (status_tx, status_rx) = watch::channel(status);
            tokio::spawn(async move {
                while let Some(command) = command_rx.recv().await {
                    match command {
                        AgentCommand::GetStatus { reply } => {
                            if !status_delay.is_zero() {
                                tokio::time::sleep(status_delay).await;
                            }
                            let _ = reply.send(status_rx.borrow().clone());
                        }
                        AgentCommand::ExecuteCorrelatedControlAction { reply, .. } => {
                            let _ = reply.send(Err(KubericError::Closed));
                        }
                        AgentCommand::ExecuteLifecycleStage { reply, .. } => {
                            let _ = reply.send(Err(KubericError::Closed));
                        }
                        AgentCommand::GetLifecycleStatus { reply, .. } => {
                            let _ = reply.send(Err(KubericError::Closed));
                        }
                    }
                }
            });
            let shutdown = CancellationToken::new();
            let server_shutdown = shutdown.child_token();
            tokio::spawn(async move {
                Server::builder()
                    .add_service(ReplicatorControlServer::new(ControlServer::new(command_tx)))
                    .serve_with_incoming_shutdown(
                        tokio_stream::wrappers::TcpListenerStream::new(listener),
                        server_shutdown.cancelled(),
                    )
                    .await
                    .unwrap();
            });
            Self {
                address,
                status_tx,
                shutdown,
            }
        }
    }

    impl Drop for SenderStatusServer {
        fn drop(&mut self) {
            self.shutdown.cancel();
        }
    }

    struct RunningPeerAgent {
        command_tx: mpsc::Sender<AgentCommand>,
        runtime_rx: mpsc::Receiver<RuntimeEffectCommand>,
        status_tx: watch::Sender<RuntimeControlSnapshot>,
        generation: AgentGeneration,
        shutdown: CancellationToken,
    }

    impl RunningPeerAgent {
        fn start(role: Role, opened: bool, clock: ManualRemoveReplicaClock) -> RunningPeerAgent {
            let (command_tx, command_rx) = mpsc::channel(32);
            let (runtime_tx, runtime_rx) = mpsc::channel(32);
            let (_fault_tx, fault_rx) = mpsc::channel(16);
            let (status_tx, status_rx) = watch::channel(RuntimeControlSnapshot {
                instance_id: ReplicaInstanceId::new("target"),
                role,
                epoch: Epoch::new(4, 9),
                configuration: None,
                election_configuration: None,
                deactivation_info: None,
                build_observation: None,
                quorum_wait_observation: None,
                partition_state: opened.then(|| Arc::new(PartitionState::new())),
            });
            let shutdown = CancellationToken::new();
            let agent = ReplicaAgent::new_with_clock(
                2,
                ReplicaInstanceId::new("target"),
                command_rx,
                runtime_tx,
                status_rx,
                fault_rx,
                shutdown.child_token(),
                Arc::new(clock),
            );
            let generation = agent.generation.clone();
            tokio::spawn(agent.serve());
            Self {
                command_tx,
                runtime_rx,
                status_tx,
                generation,
                shutdown,
            }
        }

        async fn start_with_grpc(
            role: Role,
            opened: bool,
            clock: ManualRemoveReplicaClock,
        ) -> (RunningPeerAgent, String) {
            let running = Self::start(role, opened, clock);
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = format!("http://{}", listener.local_addr().unwrap());
            let shutdown = running.shutdown.child_token();
            let command_tx = running.command_tx.clone();
            tokio::spawn(async move {
                Server::builder()
                    .add_service(ReplicaLifecyclePeerServer::new(PeerServer::new(command_tx)))
                    .serve_with_incoming_shutdown(
                        tokio_stream::wrappers::TcpListenerStream::new(listener),
                        shutdown.cancelled(),
                    )
                    .await
                    .unwrap();
            });
            (running, address)
        }

        async fn execute(&self, request: PeerStageRequest) -> Result<PeerStageObservation> {
            let (reply_tx, reply_rx) = oneshot::channel();
            self.command_tx
                .send(AgentCommand::ExecuteLifecycleStage {
                    request: Box::new(request),
                    reply: reply_tx,
                })
                .await
                .unwrap();
            reply_rx.await.unwrap()
        }

        async fn status(&self) -> PeerLifecycleStatus {
            let (reply_tx, reply_rx) = oneshot::channel();
            self.command_tx
                .send(AgentCommand::GetLifecycleStatus {
                    target_replica_id: 2,
                    target_instance_id: ReplicaInstanceId::new("target"),
                    expected_generation: self.generation.clone(),
                    reply: reply_tx,
                })
                .await
                .unwrap();
            reply_rx.await.unwrap().unwrap()
        }

        async fn next_runtime_effect(&mut self) -> RuntimeEffectCommand {
            tokio::time::timeout(Duration::from_secs(2), self.runtime_rx.recv())
                .await
                .unwrap()
                .unwrap()
        }

        async fn wait_for_terminal(
            &self,
            message_id: &str,
            expected_state: PeerStageState,
        ) -> PeerStageObservation {
            tokio::time::timeout(Duration::from_secs(3), async {
                loop {
                    let status = self.status().await;
                    if let Some(observation) = status
                        .retained_terminal_actions
                        .iter()
                        .find(|observation| observation.message_id == message_id)
                    {
                        assert_eq!(observation.state, expected_state);
                        return observation.clone();
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap()
        }
    }

    impl Drop for RunningPeerAgent {
        fn drop(&mut self) {
            self.shutdown.cancel();
        }
    }

    struct HangingLifecyclePeerTransport;

    #[async_trait]
    impl LifecyclePeerTransport for HangingLifecyclePeerTransport {
        async fn get_status(
            &self,
            _intent: &RemoveReplicaIntent,
            _timeout: Duration,
        ) -> std::result::Result<PeerLifecycleStatus, LifecyclePeerTransportError> {
            std::future::pending().await
        }

        async fn execute_stage(
            &self,
            _intent: &RemoveReplicaIntent,
            _request: PeerStageRequest,
            _timeout: Duration,
        ) -> std::result::Result<PeerStageObservation, LifecyclePeerTransportError> {
            std::future::pending().await
        }
    }

    struct ScriptedLifecyclePeerTransport {
        status: std::result::Result<PeerLifecycleStatus, LifecyclePeerTransportError>,
    }

    impl ScriptedLifecyclePeerTransport {
        fn stale(message: &str) -> Arc<Self> {
            Arc::new(Self {
                status: Err(LifecyclePeerTransportError::stale(message)),
            })
        }

        fn returning(status: PeerLifecycleStatus) -> Arc<Self> {
            Arc::new(Self { status: Ok(status) })
        }
    }

    #[async_trait]
    impl LifecyclePeerTransport for ScriptedLifecyclePeerTransport {
        async fn get_status(
            &self,
            _intent: &RemoveReplicaIntent,
            _timeout: Duration,
        ) -> std::result::Result<PeerLifecycleStatus, LifecyclePeerTransportError> {
            self.status.clone()
        }

        async fn execute_stage(
            &self,
            _intent: &RemoveReplicaIntent,
            _request: PeerStageRequest,
            _timeout: Duration,
        ) -> std::result::Result<PeerStageObservation, LifecyclePeerTransportError> {
            Err(LifecyclePeerTransportError::unavailable(
                "scripted lifecycle peer did not expect a stage dispatch",
            ))
        }
    }

    #[derive(Clone, Copy)]
    enum InitialRemoveConfiguration {
        Previous,
        CatchUp,
        Current,
    }

    struct RunningRemoveAgent {
        command_tx: mpsc::Sender<AgentCommand>,
        runtime_rx: mpsc::Receiver<RuntimeEffectCommand>,
        status_tx: watch::Sender<RuntimeControlSnapshot>,
        intent: RemoveReplicaIntent,
        shutdown: CancellationToken,
    }

    #[allow(clippy::type_complexity)]
    fn build_remove_agent(
        initial_configuration: InitialRemoveConfiguration,
        mode: RemoveReplicaMode,
        freeze_peer_authority: bool,
        connections: HashMap<ReplicaId, ReplicaInstanceId>,
        clock: ManualRemoveReplicaClock,
        lifecycle_peer_transport: Arc<dyn LifecyclePeerTransport>,
    ) -> (
        ReplicaAgent,
        mpsc::Receiver<RuntimeEffectCommand>,
        watch::Sender<RuntimeControlSnapshot>,
        RemoveReplicaIntent,
        CorrelatedControlActionRequest,
        CancellationToken,
    ) {
        let (_command_tx, command_rx) = mpsc::channel(64);
        let (runtime_tx, runtime_rx) = mpsc::channel(64);
        let (_fault_tx, fault_rx) = mpsc::channel(16);
        let state = Arc::new(PartitionState::new());
        state.set_write_status(AccessStatus::Granted);
        state.set_active_replica_connections(connections);
        let (status_tx, status_rx) = watch::channel(RuntimeControlSnapshot {
            instance_id: ReplicaInstanceId::new("primary"),
            role: Role::Primary,
            epoch: Epoch::new(4, 9),
            configuration: None,
            election_configuration: None,
            deactivation_info: None,
            build_observation: None,
            quorum_wait_observation: None,
            partition_state: Some(state),
        });
        let shutdown = CancellationToken::new();
        let agent = ReplicaAgent::new_with_clock_and_transport(
            1,
            ReplicaInstanceId::new("primary"),
            command_rx,
            runtime_tx,
            status_rx,
            fault_rx,
            shutdown.child_token(),
            Arc::new(clock.clone()),
            lifecycle_peer_transport,
        );
        let intent =
            remove_intent_for_agent(&agent, mode, freeze_peer_authority, clock.unix_seconds());
        status_tx.send_modify(|status| {
            status.configuration = Some(match initial_configuration {
                InitialRemoveConfiguration::Previous => intent.previous_status(),
                InitialRemoveConfiguration::CatchUp => intent.reduced_catch_up_status(),
                InitialRemoveConfiguration::Current => intent.reduced_current_status(),
            });
        });
        let action = DurableReplicaAction::RemoveReplicaIntent {
            intent: Box::new(intent.clone()),
        };
        let request = CorrelatedControlActionRequest {
            protocol_version: CORRELATED_CONTROL_PROTOCOL_VERSION,
            action_id: intent.action_id.clone(),
            input_signature: action.signature(),
            target_replica_id: 1,
            target_instance_id: ReplicaInstanceId::new("primary"),
            expected_agent_generation: agent.generation.clone(),
            expected_control_version: agent.control_version,
            observed_runtime_epoch: intent.epoch,
            action,
        };
        (agent, runtime_rx, status_tx, intent, request, shutdown)
    }

    impl RunningRemoveAgent {
        async fn start(
            initial_configuration: InitialRemoveConfiguration,
            mode: RemoveReplicaMode,
            freeze_peer_authority: bool,
            connections: HashMap<ReplicaId, ReplicaInstanceId>,
            clock: ManualRemoveReplicaClock,
            lifecycle_peer_transport: Arc<dyn LifecyclePeerTransport>,
        ) -> Self {
            let (command_tx, command_rx) = mpsc::channel(64);
            let (mut agent, runtime_rx, status_tx, intent, request, shutdown) = build_remove_agent(
                initial_configuration,
                mode,
                freeze_peer_authority,
                connections,
                clock.clone(),
                lifecycle_peer_transport,
            );
            agent.command_rx = command_rx;
            let (reply_tx, reply_rx) = oneshot::channel();
            agent.accept_remove_replica_intent(request.clone(), reply_tx);
            let acknowledgement = reply_rx.await.unwrap().unwrap();
            assert_eq!(
                acknowledgement.observation.action.state,
                DurableActionState::InProgress
            );
            tokio::spawn(agent.serve());
            Self {
                command_tx,
                runtime_rx,
                status_tx,
                intent,
                shutdown,
            }
        }

        async fn status(&self) -> ReplicaStatusInfo {
            let (reply_tx, reply_rx) = oneshot::channel();
            self.command_tx
                .send(AgentCommand::GetStatus { reply: reply_tx })
                .await
                .unwrap();
            reply_rx.await.unwrap()
        }

        async fn next_effect(&mut self) -> RuntimeEffectCommand {
            tokio::time::timeout(Duration::from_secs(2), self.runtime_rx.recv())
                .await
                .unwrap()
                .unwrap()
        }

        fn set_configuration(&self, configuration: ReplicaConfigurationStatus) {
            self.status_tx
                .send_modify(|status| status.configuration = Some(configuration));
        }

        fn set_quorum_observation(
            &self,
            execution_id: &str,
            state: crate::add_replica::RuntimeBuildState,
            error: Option<&str>,
        ) {
            self.status_tx.send_modify(|status| {
                status.quorum_wait_observation =
                    Some(crate::add_replica::RuntimeQuorumWaitObservation {
                        execution_id: execution_id.to_string(),
                        state,
                        error: error.map(ToOwned::to_owned),
                    });
            });
        }

        fn set_connections(&self, connections: HashMap<ReplicaId, ReplicaInstanceId>) {
            self.status_tx
                .borrow()
                .partition_state
                .as_ref()
                .unwrap()
                .set_active_replica_connections(connections);
            self.status_tx.send_modify(|_| {});
        }

        async fn wait_for_terminal(&self) -> CorrelatedActionObservation {
            tokio::time::timeout(Duration::from_secs(3), async {
                loop {
                    let status = self.status().await;
                    if let Some(terminal) = status
                        .agent
                        .retained_terminal_actions
                        .iter()
                        .find(|terminal| terminal.action.action_id == self.intent.action_id)
                    {
                        return terminal.clone();
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap()
        }
    }

    impl Drop for RunningRemoveAgent {
        fn drop(&mut self) {
            self.shutdown.cancel();
        }
    }

    fn remove_member(
        id: ReplicaId,
        instance_id: &str,
        status: ReplicaStatus,
    ) -> ConfigurationMemberDescriptor {
        ConfigurationMemberDescriptor {
            id,
            instance_id: ReplicaInstanceId::new(instance_id),
            role: Role::ActiveSecondary,
            status,
            replicator_address: format!("http://replica-{id}:7001"),
            must_catch_up: false,
            progress: ConfigurationProgressSource::Frozen {
                current_progress: id * 10,
                catch_up_capability: id * 10,
            },
        }
    }

    fn remove_intent_for_agent(
        agent: &ReplicaAgent,
        mode: RemoveReplicaMode,
        freeze_peer_authority: bool,
        now: i64,
    ) -> RemoveReplicaIntent {
        let reduced = ConfigurationDescriptor {
            members: vec![remove_member(3, "retained", ReplicaStatus::Up)],
            write_quorum: 2,
        };
        let target_status = if mode == RemoveReplicaMode::ScaleDown {
            ReplicaStatus::Up
        } else {
            ReplicaStatus::Down
        };
        let mut intent = RemoveReplicaIntent {
            protocol_version: crate::remove_replica::REMOVE_REPLICA_INTENT_PROTOCOL_VERSION,
            operation_id: "remove-operation".to_string(),
            action_id: "remove-action".to_string(),
            attempt_number: 1,
            attempt_id: "remove-attempt-1".to_string(),
            input_signature: String::new(),
            mode,
            epoch: Epoch::new(4, 9),
            primary_replica_id: 1,
            primary_instance_id: ReplicaInstanceId::new("primary"),
            primary_agent_generation: agent.generation.clone(),
            primary_agent_control_version: agent.control_version,
            primary_control_address: "http://primary:7000".to_string(),
            primary_replicator_address: "http://primary:7001".to_string(),
            target_replica_id: 2,
            target_instance_id: ReplicaInstanceId::new("target"),
            expected_target_pod_uid: "target".to_string(),
            target_pod_name: "set-2".to_string(),
            expected_target_agent_generation: freeze_peer_authority.then(|| generation('b')),
            target_control_address: freeze_peer_authority.then(|| "http://target:7000".to_string()),
            target_replicator_address: freeze_peer_authority
                .then(|| "http://target:7001".to_string()),
            target_lifecycle_peer_protocol_version: freeze_peer_authority
                .then_some(REPLICA_LIFECYCLE_PEER_PROTOCOL_VERSION),
            previous_configuration: ConfigurationDescriptor {
                members: vec![
                    remove_member(2, "target", target_status),
                    remove_member(3, "retained", ReplicaStatus::Up),
                ],
                write_quorum: 2,
            },
            reduced_catch_up_configuration: reduced.clone(),
            reduced_current_configuration: reduced,
            required_write_quorum: 2,
            minimum_committed_replicas: 2,
            maximum_pre_commit_attempts: MAX_REMOVE_REPLICA_PRE_COMMIT_ATTEMPTS,
            overall_deadline_unix_seconds: now + 600,
            compensation_grace_seconds: REMOVE_REPLICA_COMPENSATION_GRACE_SECONDS,
            compensation_deadline_cap_unix_seconds: now + 630,
            call_timeout_seconds: REMOVE_REPLICA_CALL_TIMEOUT_SECONDS,
            target_retirement_timeout_seconds: REMOVE_REPLICA_RETIREMENT_TIMEOUT_SECONDS,
        };
        intent.input_signature = intent.signature();
        intent
    }

    fn completed_live_retire_status() -> PeerLifecycleStatus {
        PeerLifecycleStatus {
            protocol_version: REPLICA_LIFECYCLE_PEER_PROTOCOL_VERSION,
            target_replica_id: 2,
            target_instance_id: ReplicaInstanceId::new("target"),
            agent_generation: generation('b'),
            peer_control_version: 1,
            role: Role::ActiveSecondary,
            epoch: Epoch::new(4, 9),
            healthy: true,
            current_progress: 20,
            current_action: Some(PeerStageObservation {
                protocol_version: REPLICA_LIFECYCLE_PEER_PROTOCOL_VERSION,
                operation_kind: PeerOperationKind::Remove,
                stage_semantic_version: RETIRE_STAGE_SEMANTIC_VERSION,
                message_id: "remove-attempt-1:Retire".to_string(),
                input_signature: "completed-live-retire".to_string(),
                stage: PeerStage::Retire,
                state: PeerStageState::Completed,
                target_agent_generation: generation('b'),
                target_peer_control_version: 1,
                error: None,
            }),
            retained_terminal_actions: Vec::new(),
        }
    }

    fn retire_request(
        target: &RunningPeerAgent,
        sender: &SenderStatusServer,
        peer_control_version: u64,
        message_id: &str,
    ) -> PeerStageRequest {
        let reduced_projection = reduced_current_projection();
        let mut request = PeerStageRequest {
            protocol_version: REPLICA_LIFECYCLE_PEER_PROTOCOL_VERSION,
            operation_kind: PeerOperationKind::Remove,
            stage_semantic_version: RETIRE_STAGE_SEMANTIC_VERSION,
            operation_id: "remove-operation".to_string(),
            attempt_id: "attempt-1".to_string(),
            message_id: message_id.to_string(),
            input_signature: String::new(),
            stage: PeerStage::Retire,
            sender_replica_id: 1,
            sender_instance_id: ReplicaInstanceId::new("primary"),
            sender_agent_generation: generation('a'),
            sender_control_address: sender.address.clone(),
            parent_action_id: "remove-parent".to_string(),
            parent_action_signature: retire_parent_signature(&reduced_projection),
            target_replica_id: 2,
            target_instance_id: ReplicaInstanceId::new("target"),
            expected_target_agent_generation: target.generation.clone(),
            expected_target_peer_control_version: peer_control_version,
            epoch: Epoch::new(4, 9),
            configuration_fence: retire_configuration_fence(&reduced_projection),
            build_key: None,
            copy_lsn: None,
            removal_mode: Some(RemoveReplicaMode::ScaleDown),
            commit_observed_unix_seconds: Some(100),
            retirement_expiry_unix_seconds: Some(160),
            reduced_current_projection: Some(reduced_projection),
        };
        request.input_signature = request.signature();
        request
    }

    async fn expect_stale_authorization_without_effect(
        target: &mut RunningPeerAgent,
        sender: &SenderStatusServer,
        base_status: &ReplicaStatusInfo,
        message_id: &str,
        mutate: impl FnOnce(&mut ReplicaStatusInfo),
    ) -> PeerStageObservation {
        let mut status = base_status.clone();
        mutate(&mut status);
        sender.status_tx.send_replace(status);
        let peer_control_version = target.status().await.peer_control_version;
        let request = retire_request(target, sender, peer_control_version, message_id);
        let accepted = target.execute(request).await.unwrap();
        assert_eq!(accepted.state, PeerStageState::Accepted);
        let terminal = target
            .wait_for_terminal(message_id, PeerStageState::Stale)
            .await;
        assert!(target.runtime_rx.try_recv().is_err());
        sender.status_tx.send_replace(base_status.clone());
        terminal
    }

    #[test]
    fn normalized_error_is_utf8_safe_and_bounded() {
        let normalized = normalize_error(&"é".repeat(MAX_ERROR_BYTES));
        assert!(normalized.len() <= MAX_ERROR_BYTES);
        assert!(normalized.is_char_boundary(normalized.len()));
        assert_eq!(
            normalize_error(&"x".repeat(MAX_ERROR_BYTES + 20)).len(),
            MAX_ERROR_BYTES
        );
    }

    #[test]
    fn agent_generations_are_process_local() {
        assert_ne!(AgentGeneration::generate(), AgentGeneration::generate());
    }

    #[tokio::test]
    async fn unsupported_protocol_and_all_fences_fail_before_effects() {
        let (mut agent, mut runtime_rx) = test_agent(Epoch::new(3, 4), true);

        let mut unsupported = request(
            &agent,
            "unsupported",
            DurableReplicaAction::RevokeWriteStatus,
        );
        unsupported.protocol_version += 1;
        assert!(matches!(
            accept(&mut agent, unsupported).await,
            Err(KubericError::UnsupportedControlProtocolVersion { .. })
        ));

        let mut target = request(&agent, "target", DurableReplicaAction::RevokeWriteStatus);
        target.target_instance_id = ReplicaInstanceId::new("replacement");
        assert!(matches!(
            accept(&mut agent, target).await,
            Err(KubericError::CorrelatedTargetMismatch { .. })
        ));

        let mut generation = request(
            &agent,
            "generation",
            DurableReplicaAction::RevokeWriteStatus,
        );
        generation.expected_agent_generation = AgentGeneration::generate();
        assert!(matches!(
            accept(&mut agent, generation).await,
            Err(KubericError::StaleAgentGeneration { .. })
        ));

        let mut epoch = request(&agent, "epoch", DurableReplicaAction::RevokeWriteStatus);
        epoch.observed_runtime_epoch = Epoch::new(3, 3);
        assert!(matches!(
            accept(&mut agent, epoch).await,
            Err(KubericError::StaleEpoch { .. })
        ));

        let mut version = request(&agent, "version", DurableReplicaAction::RevokeWriteStatus);
        version.expected_control_version = AgentControlVersion::new(1);
        assert!(matches!(
            accept(&mut agent, version).await,
            Err(KubericError::StaleAgentControlVersion { .. })
        ));
        assert_eq!(agent.control_version.value(), 0);
        assert!(runtime_rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn duplicate_replays_and_conflict_never_redispatches() {
        let (mut agent, mut runtime_rx) = test_agent(Epoch::default(), true);
        let original = request(&agent, "action", DurableReplicaAction::RevokeWriteStatus);
        let duplicate = original.clone();
        let (first_tx, first_rx) = oneshot::channel();
        agent.accept_correlated(original, first_tx);
        let runtime = runtime_rx.recv().await.unwrap();

        let replay = accept(&mut agent, duplicate.clone()).await.unwrap();
        assert_eq!(
            replay.observation.action.state,
            DurableActionState::InProgress
        );
        assert!(runtime_rx.try_recv().is_err());

        let mut conflict = duplicate;
        conflict.input_signature = DurableReplicaAction::Close.signature();
        assert!(matches!(
            accept(&mut agent, conflict).await,
            Err(KubericError::ActionSignatureMismatch { .. })
        ));

        runtime.reply.send(Ok(RuntimeEffectResult::Unit)).unwrap();
        let completion = agent.completion_rx.recv().await.unwrap();
        agent.handle_completion(completion);
        let completed = first_rx.await.unwrap().unwrap();
        assert_eq!(
            completed.observation.action.state,
            DurableActionState::Completed
        );

        let replay = request(&agent, "action", DurableReplicaAction::RevokeWriteStatus);
        let replay = accept(&mut agent, replay).await.unwrap();
        assert_eq!(
            replay.observation.action.state,
            DurableActionState::Completed
        );
        assert!(runtime_rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn reused_action_id_with_valid_different_signature_is_conflict() {
        let (mut agent, mut runtime_rx) = test_agent(Epoch::default(), true);
        agent.retain_terminal(CorrelatedActionObservation {
            generation: agent.generation.clone(),
            control_version: AgentControlVersion::new(1),
            action: DurableActionObservation {
                action_id: "action".to_string(),
                signature: DurableReplicaAction::Close.signature(),
                state: DurableActionState::Completed,
                error_class: None,
                error: None,
                result: None,
                add_replica_progress: None,
                remove_replica_progress: None,
            },
        });
        let request = request(&agent, "action", DurableReplicaAction::RevokeWriteStatus);
        assert!(matches!(
            accept(&mut agent, request).await,
            Err(KubericError::ActionIdConflict { .. })
        ));
        assert!(runtime_rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn data_loss_payload_epoch_mismatch_is_retained_failed_observation() {
        let runtime_epoch = Epoch::new(2, 7);
        let (mut agent, mut runtime_rx) = test_agent(runtime_epoch, true);
        let request = request(
            &agent,
            "data-loss",
            DurableReplicaAction::OnDataLoss {
                epoch: Epoch::new(1, 7),
            },
        );
        let acknowledgement = accept(&mut agent, request).await.unwrap();
        assert_eq!(
            acknowledgement.observation.action.state,
            DurableActionState::Failed
        );
        assert!(
            acknowledgement
                .observation
                .action
                .error
                .as_deref()
                .unwrap()
                .contains("does not match runtime epoch")
        );
        assert!(runtime_rx.try_recv().is_err());
        assert_eq!(agent.terminals.len(), 1);
    }

    #[tokio::test]
    async fn terminal_runtime_error_preserves_stable_classification() {
        let (mut agent, mut runtime_rx) = test_agent(Epoch::default(), true);
        let request = request(
            &agent,
            "quorum-failure",
            DurableReplicaAction::WaitForCatchUpQuorum {
                mode: crate::types::ReplicaSetQuorumMode::Write,
            },
        );
        let (reply_tx, reply_rx) = oneshot::channel();
        agent.accept_correlated(request, reply_tx);
        let runtime = runtime_rx.recv().await.unwrap();
        runtime
            .reply
            .send(Err(KubericError::NoWriteQuorum))
            .unwrap();
        let completion = agent.completion_rx.recv().await.unwrap();
        agent.handle_completion(completion);

        let acknowledgement = reply_rx.await.unwrap().unwrap();
        assert_eq!(
            acknowledgement.observation.action.error_class,
            Some(DurableActionErrorClass::NoWriteQuorum)
        );
        assert_eq!(
            agent.terminals.back().unwrap().action.error_class,
            Some(DurableActionErrorClass::NoWriteQuorum)
        );
    }

    #[test]
    fn terminal_and_fault_retention_are_bounded_and_deterministic() {
        let (mut agent, _runtime_rx) = test_agent(Epoch::default(), false);
        for index in 0..=TERMINAL_RETENTION {
            agent.retain_terminal(terminal(&format!("action-{index}")));
            agent.record_fault(if index % 2 == 0 {
                FaultType::Transient
            } else {
                FaultType::Permanent
            });
        }
        assert_eq!(agent.terminals.len(), TERMINAL_RETENTION);
        assert_eq!(
            agent.terminals.front().unwrap().action.action_id,
            "action-1"
        );
        assert!(agent.find_correlated("action-0").is_none());
        assert_eq!(agent.faults.len(), FAULT_RETENTION);
        assert_eq!(agent.faults.front().unwrap().sequence, 2);
    }

    #[tokio::test]
    async fn unretained_old_version_fails_as_continuity_unavailable() {
        let (mut agent, mut runtime_rx) = test_agent(Epoch::default(), true);
        agent.control_version = AgentControlVersion::new(1);
        let mut request = request(&agent, "evicted", DurableReplicaAction::RevokeWriteStatus);
        request.expected_control_version = AgentControlVersion::new(0);
        assert!(matches!(
            accept(&mut agent, request).await,
            Err(KubericError::CorrelatedContinuityUnavailable { .. })
        ));
        assert!(runtime_rx.try_recv().is_err());
    }

    #[test]
    fn status_is_required_generation_qualified_and_restart_local() {
        let (mut old_agent, _old_runtime) = test_agent(Epoch::default(), false);
        old_agent.retain_terminal(terminal("old-action"));
        let old_status = old_agent.status().agent;

        let (new_agent, _new_runtime) = test_agent(Epoch::default(), false);
        let new_status = new_agent.status().agent;
        assert_eq!(
            new_status.protocol_version,
            CORRELATED_CONTROL_PROTOCOL_VERSION
        );
        assert_ne!(old_status.generation, new_status.generation);
        assert_eq!(new_status.control_version.value(), 0);
        assert!(new_status.retained_terminal_actions.is_empty());
    }

    #[tokio::test]
    async fn same_pod_restart_rejects_prior_generation_after_ambiguous_completion() {
        let (mut old_agent, mut old_runtime_rx) = test_agent(Epoch::default(), true);
        let request = request(
            &old_agent,
            "ambiguous",
            DurableReplicaAction::RevokeWriteStatus,
        );
        let retry = request.clone();
        let (reply_tx, reply_rx) = oneshot::channel();
        old_agent.accept_correlated(request, reply_tx);
        drop(reply_rx);
        let runtime = old_runtime_rx.recv().await.unwrap();
        runtime.reply.send(Ok(RuntimeEffectResult::Unit)).unwrap();
        let completion = old_agent.completion_rx.recv().await.unwrap();
        old_agent.handle_completion(completion);

        let (mut new_agent, mut new_runtime_rx) = test_agent(Epoch::default(), true);
        assert_eq!(new_agent.instance_id, old_agent.instance_id);
        assert_ne!(new_agent.generation, old_agent.generation);
        assert!(matches!(
            accept(&mut new_agent, retry).await,
            Err(KubericError::StaleAgentGeneration { .. })
        ));
        assert!(new_runtime_rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn status_remains_available_while_effect_is_pending() {
        let (mut agent, mut runtime_rx) = test_agent(Epoch::new(1, 2), true);
        let request = request(&agent, "pending", DurableReplicaAction::RevokeWriteStatus);
        let (reply_tx, _reply_rx) = oneshot::channel();
        agent.accept_correlated(request, reply_tx);
        assert!(runtime_rx.recv().await.is_some());
        let status = agent.status();
        assert_eq!(status.epoch, Epoch::new(1, 2));
        assert_eq!(
            status.agent.current_action.unwrap().action.state,
            DurableActionState::InProgress
        );
    }

    #[tokio::test]
    async fn mismatched_and_late_completions_cannot_replace_active_state() {
        let (mut agent, mut runtime_rx) = test_agent(Epoch::default(), true);
        let request = request(&agent, "active", DurableReplicaAction::RevokeWriteStatus);
        let (reply_tx, reply_rx) = oneshot::channel();
        agent.accept_correlated(request, reply_tx);
        let runtime = runtime_rx.recv().await.unwrap();
        let active_id = agent.active.as_ref().unwrap().execution_id.clone();
        agent.handle_completion(RuntimeCompletion {
            execution_id: RuntimeEffectExecutionId {
                generation: active_id.generation.clone(),
                sequence: active_id.sequence + 1,
            },
            result: Ok(RuntimeEffectResult::Unit),
        });
        assert!(agent.active.is_some());

        runtime.reply.send(Ok(RuntimeEffectResult::Unit)).unwrap();
        let completion = agent.completion_rx.recv().await.unwrap();
        agent.handle_completion(completion);
        assert_eq!(
            reply_rx.await.unwrap().unwrap().observation.action.state,
            DurableActionState::Completed
        );
        assert!(agent.active.is_none());

        agent.handle_completion(RuntimeCompletion {
            execution_id: active_id,
            result: Ok(RuntimeEffectResult::Unit),
        });
        assert!(agent.active.is_none());
    }

    #[tokio::test]
    async fn remove_admission_is_exact_and_control_v2_is_unsupported() {
        let clock = ManualRemoveReplicaClock::new(1_000);
        let (mut agent, mut runtime_rx, _status_tx, intent, request, shutdown) = build_remove_agent(
            InitialRemoveConfiguration::Previous,
            RemoveReplicaMode::Force,
            false,
            HashMap::new(),
            clock,
            Arc::new(UnavailableLifecyclePeerTransport),
        );

        let mut active_v2 = request.clone();
        active_v2.protocol_version = CORRELATED_CONTROL_PROTOCOL_VERSION - 1;
        assert!(matches!(
            accept(&mut agent, active_v2).await,
            Err(KubericError::UnsupportedControlProtocolVersion { .. })
        ));
        assert_eq!(agent.control_version.value(), 0);
        assert!(runtime_rx.try_recv().is_err());

        let (first_tx, first_rx) = oneshot::channel();
        agent.accept_remove_replica_intent(request.clone(), first_tx);
        let first = first_rx.await.unwrap().unwrap();
        assert_eq!(
            first.observation.action.state,
            DurableActionState::InProgress
        );
        assert_eq!(
            first
                .observation
                .action
                .remove_replica_progress
                .as_ref()
                .unwrap()
                .phase,
            RemoveReplicaCoordinatorPhase::Validating
        );

        let stale_progress = RemoveReplicaProgress {
            phase: RemoveReplicaCoordinatorPhase::InstallingCatchUpConfiguration,
            attempt_id: intent.attempt_id.clone(),
            commit_observed: false,
            commit_observed_unix_seconds: None,
            connection_absent: false,
            target_retirement: TargetRetirementObservation::NotAttempted,
            retirement_expiry_unix_seconds: None,
            compensation_expiry_unix_seconds: None,
            error: None,
            current_install_dispatched: false,
        };
        agent.handle_coordinator_event(CoordinatorEvent {
            execution_id: CoordinatorExecutionId {
                generation: agent.generation.clone(),
                action_id: intent.action_id.clone(),
                attempt_id: intent.attempt_id.clone(),
                phase_sequence: 2,
            },
            update: CoordinatorUpdate::RemoveProgress(stale_progress),
        });
        assert_eq!(
            agent
                .active
                .as_ref()
                .unwrap()
                .observation
                .action
                .remove_replica_progress
                .as_ref()
                .unwrap()
                .phase,
            RemoveReplicaCoordinatorPhase::Validating
        );

        let compensation_progress = RemoveReplicaProgress {
            phase: RemoveReplicaCoordinatorPhase::Compensating,
            attempt_id: intent.attempt_id.clone(),
            commit_observed: false,
            commit_observed_unix_seconds: None,
            connection_absent: false,
            target_retirement: TargetRetirementObservation::NotAttempted,
            retirement_expiry_unix_seconds: None,
            compensation_expiry_unix_seconds: Some(1_030),
            error: Some("failure".to_string()),
            current_install_dispatched: false,
        };
        agent
            .active
            .as_mut()
            .unwrap()
            .observation
            .action
            .remove_replica_progress = Some(compensation_progress.clone());
        let (duplicate_tx, duplicate_rx) = oneshot::channel();
        agent.accept_remove_replica_intent(request.clone(), duplicate_tx);
        assert_eq!(
            duplicate_rx
                .await
                .unwrap()
                .unwrap()
                .observation
                .action
                .remove_replica_progress,
            Some(compensation_progress)
        );

        let mut conflicting_intent = intent;
        conflicting_intent.target_pod_name.push_str("-changed");
        conflicting_intent.input_signature = conflicting_intent.signature();
        let conflicting_action = DurableReplicaAction::RemoveReplicaIntent {
            intent: Box::new(conflicting_intent),
        };
        let mut conflict = request;
        conflict.input_signature = conflicting_action.signature();
        conflict.action = conflicting_action;
        let (conflict_tx, conflict_rx) = oneshot::channel();
        agent.accept_remove_replica_intent(conflict, conflict_tx);
        assert!(matches!(
            conflict_rx.await.unwrap(),
            Err(KubericError::ActionIdConflict { .. })
        ));
        assert!(runtime_rx.try_recv().is_err());
        shutdown.cancel();

        let (mut unopened, mut unopened_runtime) = test_agent(Epoch::new(4, 9), false);
        let mut unopened_intent =
            remove_intent_for_agent(&unopened, RemoveReplicaMode::Force, false, 1_000);
        unopened_intent.primary_instance_id = unopened.instance_id.clone();
        unopened_intent.input_signature = unopened_intent.signature();
        let action = DurableReplicaAction::RemoveReplicaIntent {
            intent: Box::new(unopened_intent.clone()),
        };
        let unopened_request = CorrelatedControlActionRequest {
            protocol_version: CORRELATED_CONTROL_PROTOCOL_VERSION,
            action_id: unopened_intent.action_id,
            input_signature: action.signature(),
            target_replica_id: unopened.replica_id,
            target_instance_id: unopened.instance_id.clone(),
            expected_agent_generation: unopened.generation.clone(),
            expected_control_version: unopened.control_version,
            observed_runtime_epoch: Epoch::new(4, 9),
            action,
        };
        let (reply_tx, reply_rx) = oneshot::channel();
        unopened.accept_remove_replica_intent(unopened_request, reply_tx);
        let rejected = reply_rx.await.unwrap().unwrap();
        assert_eq!(
            rejected.observation.action.state,
            DurableActionState::Failed
        );
        assert_eq!(
            rejected.observation.action.error_class,
            Some(DurableActionErrorClass::Closed)
        );
        assert!(unopened_runtime.try_recv().is_err());
    }

    #[tokio::test]
    async fn invalid_remove_progress_and_terminal_events_fail_closed() {
        let clock = ManualRemoveReplicaClock::new(1_000);
        let (mut progress_agent, _runtime_rx, _status_tx, intent, request, progress_shutdown) =
            build_remove_agent(
                InitialRemoveConfiguration::Previous,
                RemoveReplicaMode::Force,
                false,
                HashMap::new(),
                clock.clone(),
                Arc::new(UnavailableLifecyclePeerTransport),
            );
        let (reply_tx, reply_rx) = oneshot::channel();
        progress_agent.accept_remove_replica_intent(request, reply_tx);
        reply_rx.await.unwrap().unwrap();
        progress_agent.handle_coordinator_event(CoordinatorEvent {
            execution_id: CoordinatorExecutionId {
                generation: progress_agent.generation.clone(),
                action_id: intent.action_id.clone(),
                attempt_id: intent.attempt_id.clone(),
                phase_sequence: 1,
            },
            update: CoordinatorUpdate::RemoveProgress(RemoveReplicaProgress {
                phase: RemoveReplicaCoordinatorPhase::Attesting,
                attempt_id: intent.attempt_id.clone(),
                commit_observed: true,
                commit_observed_unix_seconds: Some(1_000),
                connection_absent: true,
                target_retirement: TargetRetirementObservation::Completed,
                retirement_expiry_unix_seconds: Some(1_060),
                compensation_expiry_unix_seconds: Some(1_030),
                error: None,
                current_install_dispatched: true,
            }),
        });
        assert!(progress_agent.active.is_none());
        assert!(
            progress_agent
                .terminals
                .back()
                .unwrap()
                .action
                .error
                .as_deref()
                .unwrap()
                .contains("invalid remove-replica coordinator progress")
        );
        progress_shutdown.cancel();

        let (mut terminal_agent, _runtime_rx, _status_tx, intent, request, terminal_shutdown) =
            build_remove_agent(
                InitialRemoveConfiguration::Current,
                RemoveReplicaMode::Force,
                false,
                HashMap::new(),
                clock,
                Arc::new(UnavailableLifecyclePeerTransport),
            );
        let (reply_tx, reply_rx) = oneshot::channel();
        terminal_agent.accept_remove_replica_intent(request, reply_tx);
        reply_rx.await.unwrap().unwrap();
        terminal_agent.handle_coordinator_event(CoordinatorEvent {
            execution_id: CoordinatorExecutionId {
                generation: terminal_agent.generation.clone(),
                action_id: intent.action_id.clone(),
                attempt_id: intent.attempt_id.clone(),
                phase_sequence: 1,
            },
            update: CoordinatorUpdate::RemoveProgress(RemoveReplicaProgress {
                phase: RemoveReplicaCoordinatorPhase::RemovingConnection,
                attempt_id: intent.attempt_id.clone(),
                commit_observed: true,
                commit_observed_unix_seconds: Some(1_000),
                connection_absent: false,
                target_retirement: TargetRetirementObservation::NotAttempted,
                retirement_expiry_unix_seconds: Some(1_060),
                compensation_expiry_unix_seconds: None,
                error: None,
                current_install_dispatched: true,
            }),
        });
        assert!(terminal_agent.active.is_some());
        terminal_agent.handle_coordinator_event(CoordinatorEvent {
            execution_id: CoordinatorExecutionId {
                generation: terminal_agent.generation.clone(),
                action_id: intent.action_id,
                attempt_id: intent.attempt_id,
                phase_sequence: 1,
            },
            update: CoordinatorUpdate::RemoveTerminal(Ok(
                RemoveReplicaTerminalResult::CommittedClean,
            )),
        });
        assert!(terminal_agent.active.is_none());
        assert!(
            terminal_agent
                .terminals
                .back()
                .unwrap()
                .action
                .error
                .as_deref()
                .unwrap()
                .contains("invalid remove-replica coordinator terminal")
        );
        terminal_shutdown.cancel();
    }

    #[tokio::test]
    async fn remove_coordinator_lost_replies_commit_once_and_preserve_replacement_connection() {
        let clock = ManualRemoveReplicaClock::new(1_000);
        let mut connections = HashMap::new();
        connections.insert(2, ReplicaInstanceId::new("target"));
        let mut running = RunningRemoveAgent::start(
            InitialRemoveConfiguration::Previous,
            RemoveReplicaMode::Force,
            false,
            connections,
            clock,
            Arc::new(UnavailableLifecyclePeerTransport),
        )
        .await;

        let catch_up = running.next_effect().await;
        assert!(matches!(
            catch_up.effect,
            RuntimeEffect::UpdateCatchUpConfiguration { .. }
        ));
        running.set_configuration(running.intent.reduced_catch_up_status());
        drop(catch_up.reply);

        let quorum = running.next_effect().await;
        let wait_id = match &quorum.effect {
            RuntimeEffect::StartTrackedCatchUpQuorum { execution_id, mode } => {
                assert_eq!(*mode, ReplicaSetQuorumMode::Write);
                execution_id.clone()
            }
            _ => panic!("expected tracked quorum start"),
        };
        running.set_quorum_observation(
            &wait_id,
            crate::add_replica::RuntimeBuildState::InProgress,
            None,
        );
        quorum.reply.send(Ok(RuntimeEffectResult::Unit)).unwrap();
        running.set_quorum_observation(
            &wait_id,
            crate::add_replica::RuntimeBuildState::Completed,
            None,
        );

        let current = running.next_effect().await;
        assert!(matches!(
            current.effect,
            RuntimeEffect::UpdateCurrentConfiguration { .. }
        ));
        running.set_configuration(running.intent.reduced_current_status());
        drop(current.reply);

        let remove = running.next_effect().await;
        assert!(matches!(
            remove.effect,
            RuntimeEffect::RemoveReplica {
                replica_id: 2,
                ref instance_id,
            } if instance_id == &ReplicaInstanceId::new("target")
        ));
        running.set_connections(HashMap::from([(2, ReplicaInstanceId::new("replacement"))]));
        drop(remove.reply);

        let terminal = running.wait_for_terminal().await;
        assert_eq!(terminal.action.state, DurableActionState::Completed);
        assert_eq!(
            terminal.action.result,
            Some(DurableActionResult::RemoveReplica(
                RemoveReplicaTerminalResult::CommittedDegraded
            ))
        );
        let progress = terminal.action.remove_replica_progress.unwrap();
        assert_eq!(progress.phase, RemoveReplicaCoordinatorPhase::Attesting);
        assert_eq!(progress.commit_observed_unix_seconds, Some(1_000));
        assert_eq!(progress.retirement_expiry_unix_seconds, Some(1_060));
        assert!(progress.connection_absent);
        assert_eq!(
            progress.target_retirement,
            TargetRetirementObservation::Unavailable
        );
        assert_eq!(
            running.status().await.active_replica_connections,
            vec![crate::types::ReplicaConnectionStatus {
                id: 2,
                instance_id: ReplicaInstanceId::new("replacement"),
            }]
        );
        assert!(running.runtime_rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn remove_coordinator_resumes_from_exact_completed_phase_postconditions() {
        let clock = ManualRemoveReplicaClock::new(1_000);
        let current = RunningRemoveAgent::start(
            InitialRemoveConfiguration::Current,
            RemoveReplicaMode::Force,
            false,
            HashMap::from([(2, ReplicaInstanceId::new("replacement"))]),
            clock,
            Arc::new(UnavailableLifecyclePeerTransport),
        )
        .await;
        let terminal = current.wait_for_terminal().await;
        assert_eq!(
            terminal.action.result,
            Some(DurableActionResult::RemoveReplica(
                RemoveReplicaTerminalResult::CommittedDegraded
            ))
        );
        assert!(current.runtime_rx.is_empty());
        assert_eq!(
            current.status().await.active_replica_connections,
            vec![crate::types::ReplicaConnectionStatus {
                id: 2,
                instance_id: ReplicaInstanceId::new("replacement"),
            }]
        );

        let clock = ManualRemoveReplicaClock::new(2_000);
        let (mut agent, mut runtime_rx, status_tx, intent, request, shutdown) = build_remove_agent(
            InitialRemoveConfiguration::CatchUp,
            RemoveReplicaMode::Force,
            false,
            HashMap::new(),
            clock,
            Arc::new(UnavailableLifecyclePeerTransport),
        );
        status_tx.send_modify(|status| {
            status.quorum_wait_observation =
                Some(crate::add_replica::RuntimeQuorumWaitObservation {
                    execution_id: format!("{}:remove-quorum", intent.attempt_id),
                    state: crate::add_replica::RuntimeBuildState::Completed,
                    error: None,
                });
        });
        let (reply_tx, reply_rx) = oneshot::channel();
        agent.accept_remove_replica_intent(request, reply_tx);
        assert_eq!(
            reply_rx.await.unwrap().unwrap().observation.action.state,
            DurableActionState::InProgress
        );
        let (command_tx, command_rx) = mpsc::channel(32);
        agent.command_rx = command_rx;
        tokio::spawn(agent.serve());
        let current_effect = tokio::time::timeout(Duration::from_secs(2), runtime_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(
            current_effect.effect,
            RuntimeEffect::UpdateCurrentConfiguration { .. }
        ));
        assert!(runtime_rx.try_recv().is_err());
        status_tx
            .send_modify(|status| status.configuration = Some(intent.reduced_current_status()));
        current_effect
            .reply
            .send(Ok(RuntimeEffectResult::Unit))
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let (reply_tx, reply_rx) = oneshot::channel();
                command_tx
                    .send(AgentCommand::GetStatus { reply: reply_tx })
                    .await
                    .unwrap();
                if reply_rx
                    .await
                    .unwrap()
                    .agent
                    .retained_terminal_actions
                    .iter()
                    .any(|terminal| terminal.action.action_id == intent.action_id)
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        shutdown.cancel();
    }

    #[tokio::test]
    async fn remove_coordinator_compensates_or_reports_one_time_incomplete_expiry() {
        let clock = ManualRemoveReplicaClock::new(1_000);
        let mut compensated = RunningRemoveAgent::start(
            InitialRemoveConfiguration::CatchUp,
            RemoveReplicaMode::Force,
            false,
            HashMap::new(),
            clock,
            Arc::new(UnavailableLifecyclePeerTransport),
        )
        .await;
        let quorum = compensated.next_effect().await;
        let wait_id = match &quorum.effect {
            RuntimeEffect::StartTrackedCatchUpQuorum { execution_id, .. } => execution_id.clone(),
            _ => panic!("expected tracked quorum start"),
        };
        compensated.set_quorum_observation(
            &wait_id,
            crate::add_replica::RuntimeBuildState::InProgress,
            None,
        );
        quorum.reply.send(Ok(RuntimeEffectResult::Unit)).unwrap();
        compensated.set_quorum_observation(
            &wait_id,
            crate::add_replica::RuntimeBuildState::Failed,
            Some("quorum lost"),
        );
        let restore = compensated.next_effect().await;
        assert!(matches!(
            restore.effect,
            RuntimeEffect::UpdateCurrentConfiguration { .. }
        ));
        compensated.set_configuration(compensated.intent.previous_status());
        restore.reply.send(Ok(RuntimeEffectResult::Unit)).unwrap();
        let terminal = compensated.wait_for_terminal().await;
        assert_eq!(
            terminal.action.result,
            Some(DurableActionResult::RemoveReplica(
                RemoveReplicaTerminalResult::Compensated
            ))
        );
        let progress = terminal.action.remove_replica_progress.unwrap();
        assert_eq!(progress.phase, RemoveReplicaCoordinatorPhase::Compensating);
        assert_eq!(progress.compensation_expiry_unix_seconds, Some(1_030));
        assert!(!progress.commit_observed);

        let clock = ManualRemoveReplicaClock::new(2_000);
        let mut incomplete = RunningRemoveAgent::start(
            InitialRemoveConfiguration::CatchUp,
            RemoveReplicaMode::Force,
            false,
            HashMap::new(),
            clock.clone(),
            Arc::new(UnavailableLifecyclePeerTransport),
        )
        .await;
        let quorum = incomplete.next_effect().await;
        let wait_id = match &quorum.effect {
            RuntimeEffect::StartTrackedCatchUpQuorum { execution_id, .. } => execution_id.clone(),
            _ => panic!("expected tracked quorum start"),
        };
        incomplete.set_quorum_observation(
            &wait_id,
            crate::add_replica::RuntimeBuildState::Failed,
            Some("quorum lost"),
        );
        quorum.reply.send(Ok(RuntimeEffectResult::Unit)).unwrap();
        let restore = incomplete.next_effect().await;
        clock.advance(30);
        restore
            .reply
            .send(Err(KubericError::Internal("restore failed".into())))
            .unwrap();
        incomplete.status_tx.send_modify(|_| {});
        let terminal = incomplete.wait_for_terminal().await;
        assert_eq!(
            terminal.action.result,
            Some(DurableActionResult::RemoveReplica(
                RemoveReplicaTerminalResult::CompensationIncomplete
            ))
        );
        let progress = terminal.action.remove_replica_progress.unwrap();
        assert_eq!(progress.compensation_expiry_unix_seconds, Some(2_030));
        assert!(progress.error.unwrap().contains("restore"));
    }

    #[tokio::test]
    async fn reduced_current_dispatch_ambiguity_never_constructs_previous_configuration_effect() {
        let clock = ManualRemoveReplicaClock::new(1_000);
        let mut running = RunningRemoveAgent::start(
            InitialRemoveConfiguration::CatchUp,
            RemoveReplicaMode::Force,
            false,
            HashMap::new(),
            clock,
            Arc::new(UnavailableLifecyclePeerTransport),
        )
        .await;
        let quorum = running.next_effect().await;
        let wait_id = match &quorum.effect {
            RuntimeEffect::StartTrackedCatchUpQuorum { execution_id, .. } => execution_id.clone(),
            _ => panic!("expected tracked quorum start"),
        };
        running.set_quorum_observation(
            &wait_id,
            crate::add_replica::RuntimeBuildState::Completed,
            None,
        );
        quorum.reply.send(Ok(RuntimeEffectResult::Unit)).unwrap();
        let current = running.next_effect().await;
        assert!(matches!(
            current.effect,
            RuntimeEffect::UpdateCurrentConfiguration { .. }
        ));
        drop(current.reply);

        let terminal = running.wait_for_terminal().await;
        assert_eq!(terminal.action.state, DurableActionState::Failed);
        assert!(
            terminal
                .action
                .error
                .unwrap()
                .contains("rollback is forbidden")
        );
        assert_eq!(
            running.status_tx.borrow().configuration,
            Some(running.intent.reduced_catch_up_status())
        );
        assert!(running.runtime_rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn remove_authority_gate_rejects_every_stale_primary_boundary() {
        for mutation in 0..5 {
            let clock = ManualRemoveReplicaClock::new(1_000);
            let (agent, _runtime_rx, status_tx, intent, _request, _shutdown) = build_remove_agent(
                InitialRemoveConfiguration::Previous,
                RemoveReplicaMode::Force,
                false,
                HashMap::new(),
                clock.clone(),
                Arc::new(UnavailableLifecyclePeerTransport),
            );
            status_tx.send_modify(|status| match mutation {
                0 => status.role = Role::ActiveSecondary,
                1 => status
                    .partition_state
                    .as_ref()
                    .unwrap()
                    .set_write_status(AccessStatus::NoWriteQuorum),
                2 => status.epoch = Epoch::new(4, 10),
                3 => status.instance_id = ReplicaInstanceId::new("replacement-primary"),
                4 => {
                    status.configuration = Some(ReplicaConfigurationStatus {
                        mode: ReplicaConfigurationMode::Current,
                        members: Vec::new(),
                        write_quorum: 1,
                    })
                }
                _ => unreachable!(),
            });
            let execution = CoordinatorExecution::new(
                agent.generation.clone(),
                intent.action_id.clone(),
                intent.attempt_id.clone(),
            );
            let (_authority_tx, mut authority_rx) = watch::channel(Some(CoordinatorAuthority {
                generation: agent.generation.clone(),
                action_id: intent.action_id.clone(),
                action_signature: intent.input_signature.clone(),
                attempt_id: intent.attempt_id.clone(),
                phase_sequence: 0,
            }));
            assert!(
                remove_authority_gate(
                    &execution,
                    &intent,
                    &mut authority_rx,
                    &agent.runtime_status_rx,
                    &clock,
                    Some(intent.overall_deadline_unix_seconds),
                    &[RemoveConfigurationObservation::PreviousCurrent],
                )
                .await
                .is_err(),
                "runtime authority mutation {mutation} was accepted"
            );
        }

        for mutation in 0..5 {
            let clock = ManualRemoveReplicaClock::new(1_000);
            let (agent, _runtime_rx, _status_tx, intent, _request, _shutdown) = build_remove_agent(
                InitialRemoveConfiguration::Previous,
                RemoveReplicaMode::Force,
                false,
                HashMap::new(),
                clock.clone(),
                Arc::new(UnavailableLifecyclePeerTransport),
            );
            let execution = CoordinatorExecution::new(
                agent.generation.clone(),
                intent.action_id.clone(),
                intent.attempt_id.clone(),
            );
            let mut authority = CoordinatorAuthority {
                generation: agent.generation.clone(),
                action_id: intent.action_id.clone(),
                action_signature: intent.input_signature.clone(),
                attempt_id: intent.attempt_id.clone(),
                phase_sequence: 0,
            };
            match mutation {
                0 => authority.generation = generation('f'),
                1 => authority.action_id = "replacement-action".to_string(),
                2 => authority.action_signature = "replacement-signature".to_string(),
                3 => authority.attempt_id = "replacement-attempt".to_string(),
                4 => authority.phase_sequence = 1,
                _ => unreachable!(),
            }
            let (_authority_tx, mut authority_rx) = watch::channel(Some(authority));
            assert!(
                remove_authority_gate(
                    &execution,
                    &intent,
                    &mut authority_rx,
                    &agent.runtime_status_rx,
                    &clock,
                    Some(intent.overall_deadline_unix_seconds),
                    &[RemoveConfigurationObservation::PreviousCurrent],
                )
                .await
                .is_err(),
                "coordinator authority mutation {mutation} was accepted"
            );
        }
    }

    #[tokio::test]
    async fn remove_status_stays_responsive_during_held_quorum_and_peer_calls() {
        let clock = ManualRemoveReplicaClock::new(1_000);
        let mut quorum_wait = RunningRemoveAgent::start(
            InitialRemoveConfiguration::Previous,
            RemoveReplicaMode::Force,
            false,
            HashMap::new(),
            clock,
            Arc::new(UnavailableLifecyclePeerTransport),
        )
        .await;
        let catch_up = quorum_wait.next_effect().await;
        quorum_wait.set_configuration(quorum_wait.intent.reduced_catch_up_status());
        catch_up.reply.send(Ok(RuntimeEffectResult::Unit)).unwrap();
        let quorum = quorum_wait.next_effect().await;
        let wait_id = match &quorum.effect {
            RuntimeEffect::StartTrackedCatchUpQuorum { execution_id, .. } => execution_id.clone(),
            _ => panic!("expected tracked quorum start"),
        };
        quorum_wait.set_quorum_observation(
            &wait_id,
            crate::add_replica::RuntimeBuildState::InProgress,
            None,
        );
        quorum.reply.send(Ok(RuntimeEffectResult::Unit)).unwrap();
        for _ in 0..5 {
            let status = tokio::time::timeout(Duration::from_millis(100), quorum_wait.status())
                .await
                .unwrap();
            assert_eq!(
                status
                    .agent
                    .current_action
                    .unwrap()
                    .action
                    .remove_replica_progress
                    .unwrap()
                    .phase,
                RemoveReplicaCoordinatorPhase::WaitingForCatchUpQuorum
            );
        }

        let clock = ManualRemoveReplicaClock::new(2_000);
        let peer_transport: Arc<dyn LifecyclePeerTransport> =
            Arc::new(HangingLifecyclePeerTransport);
        let peer_wait = RunningRemoveAgent::start(
            InitialRemoveConfiguration::Current,
            RemoveReplicaMode::ScaleDown,
            true,
            HashMap::new(),
            clock.clone(),
            peer_transport,
        )
        .await;
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let status = peer_wait.status().await;
                if status
                    .agent
                    .current_action
                    .as_ref()
                    .and_then(|action| action.action.remove_replica_progress.as_ref())
                    .is_some_and(|progress| {
                        progress.phase == RemoveReplicaCoordinatorPhase::RetiringTarget
                    })
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        for _ in 0..5 {
            assert!(
                tokio::time::timeout(Duration::from_millis(100), peer_wait.status())
                    .await
                    .is_ok()
            );
        }
        clock.advance(60);
        peer_wait.status_tx.send_modify(|_| {});
        let terminal = peer_wait.wait_for_terminal().await;
        assert_eq!(
            terminal.action.result,
            Some(DurableActionResult::RemoveReplica(
                RemoveReplicaTerminalResult::CommittedDegraded
            ))
        );
    }

    #[tokio::test]
    async fn remove_commit_crossing_overall_deadline_rolls_forward_after_exact_cleanup() {
        let clock = ManualRemoveReplicaClock::new(1_000);
        let mut running = RunningRemoveAgent::start(
            InitialRemoveConfiguration::Previous,
            RemoveReplicaMode::Force,
            false,
            HashMap::from([(2, ReplicaInstanceId::new("target"))]),
            clock.clone(),
            Arc::new(UnavailableLifecyclePeerTransport),
        )
        .await;

        let catch_up = running.next_effect().await;
        running.set_configuration(running.intent.reduced_catch_up_status());
        catch_up.reply.send(Ok(RuntimeEffectResult::Unit)).unwrap();

        let quorum = running.next_effect().await;
        let wait_id = match &quorum.effect {
            RuntimeEffect::StartTrackedCatchUpQuorum { execution_id, .. } => execution_id.clone(),
            _ => panic!("expected tracked quorum start"),
        };
        running.set_quorum_observation(
            &wait_id,
            crate::add_replica::RuntimeBuildState::Completed,
            None,
        );
        quorum.reply.send(Ok(RuntimeEffectResult::Unit)).unwrap();

        let current = running.next_effect().await;
        assert!(matches!(
            current.effect,
            RuntimeEffect::UpdateCurrentConfiguration { .. }
        ));
        running.set_configuration(running.intent.reduced_current_status());
        clock.advance(601);
        drop(current.reply);

        let cleanup = running.next_effect().await;
        assert!(matches!(
            cleanup.effect,
            RuntimeEffect::RemoveReplica {
                replica_id: 2,
                ref instance_id,
            } if instance_id == &ReplicaInstanceId::new("target")
        ));
        running.set_connections(HashMap::new());
        cleanup.reply.send(Ok(RuntimeEffectResult::Unit)).unwrap();

        let terminal = running.wait_for_terminal().await;
        assert_eq!(
            terminal.action.result,
            Some(DurableActionResult::RemoveReplica(
                RemoveReplicaTerminalResult::CommittedDegraded
            ))
        );
        let progress = terminal.action.remove_replica_progress.unwrap();
        assert_eq!(progress.commit_observed_unix_seconds, Some(1_600));
        assert_eq!(progress.retirement_expiry_unix_seconds, Some(1_600));
        assert!(progress.connection_absent);
        assert_eq!(
            progress.target_retirement,
            TargetRetirementObservation::Unavailable
        );
        assert!(running.status().await.active_replica_connections.is_empty());
        assert!(running.runtime_rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn completed_peer_without_live_close_remains_in_progress_until_expiry() {
        let clock = ManualRemoveReplicaClock::new(3_000);
        let transport = ScriptedLifecyclePeerTransport::returning(completed_live_retire_status());
        let running = RunningRemoveAgent::start(
            InitialRemoveConfiguration::Current,
            RemoveReplicaMode::ScaleDown,
            true,
            HashMap::new(),
            clock.clone(),
            transport,
        )
        .await;

        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let status = running.status().await;
                if status
                    .agent
                    .current_action
                    .as_ref()
                    .and_then(|action| action.action.remove_replica_progress.as_ref())
                    .is_some_and(|progress| {
                        progress.target_retirement == TargetRetirementObservation::InProgress
                    })
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();

        clock.advance(60);
        running.status_tx.send_modify(|_| {});
        let terminal = running.wait_for_terminal().await;
        assert_eq!(
            terminal.action.result,
            Some(DurableActionResult::RemoveReplica(
                RemoveReplicaTerminalResult::CommittedDegraded
            ))
        );
        assert_eq!(
            terminal
                .action
                .remove_replica_progress
                .unwrap()
                .target_retirement,
            TargetRetirementObservation::Failed
        );
    }

    #[tokio::test]
    async fn remove_manual_clock_enforces_call_overall_retirement_and_compensation_boundaries() {
        let clock = ManualRemoveReplicaClock::new(100);
        let (runtime_tx, mut runtime_rx) = mpsc::channel(1);
        let effect_clock = clock.clone();
        let call = tokio::spawn(async move {
            execute_remove_runtime_effect(
                &runtime_tx,
                RuntimeEffect::RemoveReplica {
                    replica_id: 2,
                    instance_id: ReplicaInstanceId::new("target"),
                },
                &effect_clock,
                200,
                10,
                "test held effect",
            )
            .await
        });
        let _held = runtime_rx.recv().await.unwrap();
        clock.advance(10);
        assert!(call.await.unwrap().unwrap_err().contains("timed out"));

        let clock = ManualRemoveReplicaClock::new(1_000);
        let mut overall = RunningRemoveAgent::start(
            InitialRemoveConfiguration::Previous,
            RemoveReplicaMode::Force,
            false,
            HashMap::new(),
            clock.clone(),
            Arc::new(UnavailableLifecyclePeerTransport),
        )
        .await;
        let catch_up = overall.next_effect().await;
        overall.set_configuration(overall.intent.reduced_catch_up_status());
        clock.advance(600);
        drop(catch_up.reply);
        let restore = overall.next_effect().await;
        assert!(matches!(
            restore.effect,
            RuntimeEffect::UpdateCurrentConfiguration { .. }
        ));
        clock.advance(30);
        restore
            .reply
            .send(Err(KubericError::Internal("expired restore".into())))
            .unwrap();
        overall.status_tx.send_modify(|_| {});
        let terminal = overall.wait_for_terminal().await;
        let progress = terminal.action.remove_replica_progress.unwrap();
        assert_eq!(
            terminal.action.result,
            Some(DurableActionResult::RemoveReplica(
                RemoveReplicaTerminalResult::CompensationIncomplete
            ))
        );
        assert_eq!(progress.compensation_expiry_unix_seconds, Some(1_630));

        let clock = ManualRemoveReplicaClock::new(3_000);
        let stale_transport = ScriptedLifecyclePeerTransport::stale("stale target generation");
        let stale = RunningRemoveAgent::start(
            InitialRemoveConfiguration::Current,
            RemoveReplicaMode::Force,
            true,
            HashMap::new(),
            clock.clone(),
            stale_transport,
        )
        .await;
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let status = stale.status().await;
                if status
                    .agent
                    .current_action
                    .as_ref()
                    .and_then(|action| action.action.remove_replica_progress.as_ref())
                    .is_some_and(|progress| {
                        progress.target_retirement == TargetRetirementObservation::Stale
                    })
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        clock.advance(60);
        stale.status_tx.send_modify(|_| {});
        let terminal = stale.wait_for_terminal().await;
        let progress = terminal.action.remove_replica_progress.unwrap();
        assert_eq!(progress.retirement_expiry_unix_seconds, Some(3_060));
        assert_eq!(
            progress.target_retirement,
            TargetRetirementObservation::Stale
        );
    }

    #[tokio::test]
    async fn retire_orders_role_none_then_close_and_replays_only_live_completion() {
        let sender = SenderStatusServer::start(sender_status(reduced_current_projection())).await;
        let mut target = RunningPeerAgent::start(
            Role::ActiveSecondary,
            true,
            ManualRemoveReplicaClock::new(100),
        );
        let request = retire_request(&target, &sender, 0, "retire-ordered");

        let accepted = target.execute(request.clone()).await.unwrap();
        assert_eq!(accepted.state, PeerStageState::Accepted);
        assert_eq!(
            target.status().await.current_action.unwrap().state,
            PeerStageState::InProgress
        );

        let demote = tokio::time::timeout(Duration::from_secs(2), target.runtime_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(
            demote.effect,
            RuntimeEffect::ChangeRole {
                epoch: Epoch {
                    data_loss_number: 4,
                    configuration_number: 9
                },
                role: Role::None
            }
        ));
        assert!(target.runtime_rx.try_recv().is_err());
        target
            .status_tx
            .send_modify(|status| status.role = Role::None);
        demote.reply.send(Ok(RuntimeEffectResult::Unit)).unwrap();

        let close = tokio::time::timeout(Duration::from_secs(2), target.runtime_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(close.effect, RuntimeEffect::Close));
        target.status_tx.send_modify(|status| {
            status.role = Role::None;
            status.partition_state = None;
        });
        close.reply.send(Ok(RuntimeEffectResult::Unit)).unwrap();

        let completed = target
            .wait_for_terminal("retire-ordered", PeerStageState::Completed)
            .await;
        assert_eq!(completed.operation_kind, PeerOperationKind::Remove);
        assert_eq!(completed.stage, PeerStage::Retire);
        assert_eq!(target.execute(request.clone()).await.unwrap(), completed);
        assert!(target.runtime_rx.try_recv().is_err());

        target.status_tx.send_modify(|status| {
            status.role = Role::ActiveSecondary;
            status.partition_state = Some(Arc::new(PartitionState::new()));
        });
        let stale = target.execute(request).await.unwrap();
        assert_eq!(stale.state, PeerStageState::Stale);
        assert!(target.runtime_rx.try_recv().is_err());
        assert_eq!(
            target.status().await.retained_terminal_actions,
            vec![completed]
        );
    }

    #[tokio::test]
    async fn retire_rejection_conflict_and_unsupported_peer_v1_are_one_shot() {
        let sender = SenderStatusServer::start(sender_status(reduced_current_projection())).await;
        let mut target =
            RunningPeerAgent::start(Role::None, false, ManualRemoveReplicaClock::new(100));

        let mut rejected = retire_request(&target, &sender, 0, "retire-rejected");
        rejected.reduced_current_projection = None;
        rejected.input_signature = rejected.signature();
        let rejected = target.execute(rejected).await.unwrap();
        assert_eq!(rejected.state, PeerStageState::Rejected);
        assert!(target.status().await.retained_terminal_actions.is_empty());

        let mut active_v1 = retire_request(&target, &sender, 0, "retire-v1");
        active_v1.protocol_version = REPLICA_LIFECYCLE_PEER_PROTOCOL_VERSION - 1;
        active_v1.input_signature = active_v1.signature();
        assert!(matches!(
            target.execute(active_v1).await,
            Err(KubericError::UnsupportedPeerProtocolVersion { .. })
        ));
        assert!(target.status().await.retained_terminal_actions.is_empty());

        let request = retire_request(&target, &sender, 0, "retire-conflict");
        assert_eq!(
            target.execute(request.clone()).await.unwrap().state,
            PeerStageState::Accepted
        );
        let completed = target
            .wait_for_terminal("retire-conflict", PeerStageState::Completed)
            .await;
        assert!(target.runtime_rx.try_recv().is_err());

        let mut conflict = request;
        conflict.attempt_id = "attempt-2".to_string();
        conflict.input_signature = conflict.signature();
        let conflict = target.execute(conflict).await.unwrap();
        assert_eq!(conflict.state, PeerStageState::Conflict);
        let status = target.status().await;
        assert_eq!(status.peer_control_version, 1);
        assert_eq!(status.retained_terminal_actions, vec![completed]);

        let mut invalid_stage_version = retire_request(&target, &sender, 1, "retire-stage-version");
        invalid_stage_version.stage_semantic_version += 1;
        invalid_stage_version.input_signature = invalid_stage_version.signature();
        let rejected = target.execute(invalid_stage_version).await.unwrap();
        assert_eq!(rejected.state, PeerStageState::Rejected);
        assert_eq!(target.status().await.peer_control_version, 1);
    }

    #[tokio::test]
    async fn real_lifecycle_peer_v2_grpc_retires_exact_target() {
        let sender = SenderStatusServer::start(sender_status(reduced_current_projection())).await;
        let (mut target, address) = RunningPeerAgent::start_with_grpc(
            Role::ActiveSecondary,
            true,
            ManualRemoveReplicaClock::new(100),
        )
        .await;
        let client = crate::grpc::peer_client::GrpcPeerClient::connect(
            address,
            2,
            ReplicaInstanceId::new("target"),
            target.generation.clone(),
        )
        .await
        .unwrap();
        let request = retire_request(&target, &sender, 0, "real-retire");
        let accepted = client
            .execute_stage(request, Duration::from_secs(2))
            .await
            .unwrap();
        assert_eq!(accepted.state, PeerStageState::Accepted);

        let demote = target.next_runtime_effect().await;
        assert!(matches!(
            demote.effect,
            RuntimeEffect::ChangeRole {
                role: Role::None,
                ..
            }
        ));
        target
            .status_tx
            .send_modify(|status| status.role = Role::None);
        demote.reply.send(Ok(RuntimeEffectResult::Unit)).unwrap();
        let close = target.next_runtime_effect().await;
        assert!(matches!(close.effect, RuntimeEffect::Close));
        target.status_tx.send_modify(|status| {
            status.role = Role::None;
            status.partition_state = None;
        });
        close.reply.send(Ok(RuntimeEffectResult::Unit)).unwrap();

        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let status = client.get_status(Duration::from_secs(1)).await.unwrap();
                if status.retained_terminal_actions.iter().any(|observation| {
                    observation.message_id == "real-retire"
                        && observation.state == PeerStageState::Completed
                }) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn real_lifecycle_peer_v2_yields_committed_clean() {
        let clock = ManualRemoveReplicaClock::new(100);
        let (mut target, target_address) =
            RunningPeerAgent::start_with_grpc(Role::ActiveSecondary, true, clock.clone()).await;
        let (mut agent, _runtime_rx, _status_tx, mut intent, mut request, shutdown) =
            build_remove_agent(
                InitialRemoveConfiguration::Current,
                RemoveReplicaMode::ScaleDown,
                true,
                HashMap::new(),
                clock,
                Arc::new(GrpcLifecyclePeerTransport),
            );
        intent.expected_target_agent_generation = Some(target.generation.clone());
        intent.target_control_address = Some(target_address);

        let mut sender_runtime = sender_status(intent.reduced_current_status());
        sender_runtime.agent.generation = agent.generation.clone();
        sender_runtime
            .agent
            .current_action
            .as_mut()
            .unwrap()
            .generation = agent.generation.clone();
        let sender = SenderStatusServer::start(sender_runtime).await;
        intent.primary_control_address = sender.address.clone();
        intent.input_signature = intent.signature();
        intent.validate().unwrap();
        sender.status_tx.send_modify(|status| {
            let current = status.agent.current_action.as_mut().unwrap();
            current.action.action_id = intent.action_id.clone();
            current.action.signature = intent.input_signature.clone();
        });

        let action = DurableReplicaAction::RemoveReplicaIntent {
            intent: Box::new(intent.clone()),
        };
        request.action_id = intent.action_id.clone();
        request.input_signature = action.signature();
        request.expected_agent_generation = agent.generation.clone();
        request.expected_control_version = agent.control_version;
        request.action = action;
        let (command_tx, command_rx) = mpsc::channel(32);
        agent.command_rx = command_rx;
        let (reply_tx, reply_rx) = oneshot::channel();
        agent.accept_remove_replica_intent(request, reply_tx);
        assert_eq!(
            reply_rx.await.unwrap().unwrap().observation.action.state,
            DurableActionState::InProgress
        );
        tokio::spawn(agent.serve());

        let demote = target.next_runtime_effect().await;
        assert!(matches!(
            demote.effect,
            RuntimeEffect::ChangeRole {
                role: Role::None,
                ..
            }
        ));
        target
            .status_tx
            .send_modify(|status| status.role = Role::None);
        demote.reply.send(Ok(RuntimeEffectResult::Unit)).unwrap();
        let close = target.next_runtime_effect().await;
        assert!(matches!(close.effect, RuntimeEffect::Close));
        target.status_tx.send_modify(|status| {
            status.role = Role::None;
            status.partition_state = None;
        });
        close.reply.send(Ok(RuntimeEffectResult::Unit)).unwrap();

        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let (status_tx, status_rx) = oneshot::channel();
                command_tx
                    .send(AgentCommand::GetStatus { reply: status_tx })
                    .await
                    .unwrap();
                let status = status_rx.await.unwrap();
                if status
                    .agent
                    .retained_terminal_actions
                    .iter()
                    .any(|terminal| {
                        terminal.action.action_id == intent.action_id
                            && terminal.action.result
                                == Some(DurableActionResult::RemoveReplica(
                                    RemoveReplicaTerminalResult::CommittedClean,
                                ))
                    })
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        shutdown.cancel();
    }

    #[tokio::test]
    async fn retire_sender_parent_epoch_and_configuration_fences_are_exact() {
        let base_status = sender_status(reduced_current_projection());
        let sender = SenderStatusServer::start(base_status.clone()).await;
        let mut target = RunningPeerAgent::start(
            Role::ActiveSecondary,
            true,
            ManualRemoveReplicaClock::new(100),
        );

        expect_stale_authorization_without_effect(
            &mut target,
            &sender,
            &base_status,
            "stale-sender-instance",
            |status| status.instance_id = ReplicaInstanceId::new("replacement-primary"),
        )
        .await;
        expect_stale_authorization_without_effect(
            &mut target,
            &sender,
            &base_status,
            "stale-sender-generation",
            |status| {
                status.agent.generation = generation('b');
                status.agent.current_action.as_mut().unwrap().generation = generation('b');
            },
        )
        .await;
        expect_stale_authorization_without_effect(
            &mut target,
            &sender,
            &base_status,
            "stale-parent",
            |status| {
                status
                    .agent
                    .current_action
                    .as_mut()
                    .unwrap()
                    .action
                    .action_id = "other-parent".to_string();
            },
        )
        .await;
        expect_stale_authorization_without_effect(
            &mut target,
            &sender,
            &base_status,
            "stale-parent-signature",
            |status| {
                status
                    .agent
                    .current_action
                    .as_mut()
                    .unwrap()
                    .action
                    .signature = "other-parent-signature".to_string();
            },
        )
        .await;
        expect_stale_authorization_without_effect(
            &mut target,
            &sender,
            &base_status,
            "stale-sender-epoch",
            |status| status.epoch = Epoch::new(4, 10),
        )
        .await;
        expect_stale_authorization_without_effect(
            &mut target,
            &sender,
            &base_status,
            "stale-sender-role",
            |status| status.role = Role::ActiveSecondary,
        )
        .await;
        expect_stale_authorization_without_effect(
            &mut target,
            &sender,
            &base_status,
            "stale-write-authority",
            |status| status.write_status = AccessStatus::NoWriteQuorum,
        )
        .await;

        let catch_up = expect_stale_authorization_without_effect(
            &mut target,
            &sender,
            &base_status,
            "stale-catch-up",
            |status| {
                status.configuration.as_mut().unwrap().mode = ReplicaConfigurationMode::CatchUp
            },
        )
        .await;
        assert!(catch_up.error.unwrap().contains("CatchUp"));

        for (message_id, mutation) in [
            ("stale-member", 0_u8),
            ("stale-incarnation", 1),
            ("stale-role", 2),
            ("stale-quorum", 3),
        ] {
            expect_stale_authorization_without_effect(
                &mut target,
                &sender,
                &base_status,
                message_id,
                |status| {
                    let configuration = status.configuration.as_mut().unwrap();
                    match mutation {
                        0 => configuration.members[0].id = 4,
                        1 => {
                            configuration.members[0].instance_id =
                                ReplicaInstanceId::new("other-retained")
                        }
                        2 => configuration.members[0].role = Role::IdleSecondary,
                        3 => configuration.write_quorum = 1,
                        _ => unreachable!(),
                    }
                },
            )
            .await;
        }

        let peer_control_version = target.status().await.peer_control_version;
        let mut exact_target_present = retire_request(
            &target,
            &sender,
            peer_control_version,
            "target-still-current",
        );
        let mut status = base_status.clone();
        status.configuration = Some(ReplicaConfigurationStatus {
            mode: ReplicaConfigurationMode::Current,
            members: vec![
                ReplicaConfigurationMemberStatus {
                    id: 2,
                    instance_id: ReplicaInstanceId::new("target"),
                    role: Role::ActiveSecondary,
                },
                ReplicaConfigurationMemberStatus {
                    id: 3,
                    instance_id: ReplicaInstanceId::new("retained"),
                    role: Role::ActiveSecondary,
                },
            ],
            write_quorum: 2,
        });
        sender.status_tx.send_replace(status);
        exact_target_present.input_signature = exact_target_present.signature();
        assert_eq!(
            target.execute(exact_target_present).await.unwrap().state,
            PeerStageState::Accepted
        );
        let stale = target
            .wait_for_terminal("target-still-current", PeerStageState::Stale)
            .await;
        assert!(stale.error.unwrap().contains("exact target"));
        assert!(target.runtime_rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn retire_target_generation_and_peer_control_fences_are_exact_even_for_force() {
        let sender = SenderStatusServer::start(sender_status(reduced_current_projection())).await;
        let mut target = RunningPeerAgent::start(
            Role::ActiveSecondary,
            true,
            ManualRemoveReplicaClock::new(100),
        );

        let mut wrong_incarnation = retire_request(&target, &sender, 0, "wrong-incarnation");
        wrong_incarnation.target_instance_id = ReplicaInstanceId::new("replacement");
        wrong_incarnation.input_signature = wrong_incarnation.signature();
        assert_eq!(
            target.execute(wrong_incarnation).await.unwrap().state,
            PeerStageState::Stale
        );

        let mut wrong_generation = retire_request(&target, &sender, 0, "wrong-generation");
        wrong_generation.removal_mode = Some(RemoveReplicaMode::Force);
        wrong_generation.expected_target_agent_generation = generation('f');
        wrong_generation.input_signature = wrong_generation.signature();
        assert_eq!(
            target.execute(wrong_generation).await.unwrap().state,
            PeerStageState::Stale
        );

        let wrong_peer_version = retire_request(&target, &sender, 1, "wrong-peer-control");
        assert_eq!(
            target.execute(wrong_peer_version).await.unwrap().state,
            PeerStageState::Stale
        );
        assert_eq!(target.status().await.peer_control_version, 0);
        assert!(target.status().await.retained_terminal_actions.is_empty());
        assert!(target.runtime_rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn retire_reauthorizes_sender_after_demotion_before_close() {
        let base_status = sender_status(reduced_current_projection());
        let sender = SenderStatusServer::start(base_status.clone()).await;
        let mut target = RunningPeerAgent::start(
            Role::ActiveSecondary,
            true,
            ManualRemoveReplicaClock::new(100),
        );
        let request = retire_request(&target, &sender, 0, "revoke-before-close");
        assert_eq!(
            target.execute(request).await.unwrap().state,
            PeerStageState::Accepted
        );
        let demote = target.runtime_rx.recv().await.unwrap();
        assert!(matches!(
            demote.effect,
            RuntimeEffect::ChangeRole {
                role: Role::None,
                ..
            }
        ));
        target
            .status_tx
            .send_modify(|status| status.role = Role::None);
        let mut revoked = base_status;
        revoked.write_status = AccessStatus::NotPrimary;
        sender.status_tx.send_replace(revoked);
        demote.reply.send(Ok(RuntimeEffectResult::Unit)).unwrap();

        target
            .wait_for_terminal("revoke-before-close", PeerStageState::Stale)
            .await;
        assert!(target.runtime_rx.try_recv().is_err());
        assert!(target.status_tx.borrow().partition_state.is_some());
    }

    #[tokio::test]
    async fn retire_rejects_midflight_target_epoch_and_incarnation_changes_before_close() {
        for mutation in 0..2 {
            let sender =
                SenderStatusServer::start(sender_status(reduced_current_projection())).await;
            let mut target = RunningPeerAgent::start(
                Role::ActiveSecondary,
                true,
                ManualRemoveReplicaClock::new(100),
            );
            let message_id = format!("target-midflight-{mutation}");
            let request = retire_request(&target, &sender, 0, &message_id);
            assert_eq!(
                target.execute(request).await.unwrap().state,
                PeerStageState::Accepted
            );
            let demote = target.runtime_rx.recv().await.unwrap();
            assert!(matches!(
                demote.effect,
                RuntimeEffect::ChangeRole {
                    role: Role::None,
                    ..
                }
            ));
            target.status_tx.send_modify(|status| {
                status.role = Role::None;
                if mutation == 0 {
                    status.instance_id = ReplicaInstanceId::new("replacement-target");
                } else {
                    status.epoch = Epoch::new(4, 10);
                }
            });
            demote.reply.send(Ok(RuntimeEffectResult::Unit)).unwrap();

            let stale = target
                .wait_for_terminal(&message_id, PeerStageState::Stale)
                .await;
            assert!(stale.error.unwrap().contains("changed during Retire"));
            assert!(target.runtime_rx.try_recv().is_err());
            assert!(target.status_tx.borrow().partition_state.is_some());
        }
    }

    #[tokio::test]
    async fn retire_allows_same_id_replacement_but_never_retargets_the_old_runtime() {
        let replacement_projection = ReplicaConfigurationStatus {
            mode: ReplicaConfigurationMode::Current,
            members: vec![ReplicaConfigurationMemberStatus {
                id: 2,
                instance_id: ReplicaInstanceId::new("replacement"),
                role: Role::ActiveSecondary,
            }],
            write_quorum: 2,
        };
        let sender = SenderStatusServer::start(sender_status(replacement_projection.clone())).await;
        let mut target = RunningPeerAgent::start(
            Role::ActiveSecondary,
            true,
            ManualRemoveReplicaClock::new(100),
        );
        let mut request = retire_request(&target, &sender, 0, "retire-old-incarnation");
        request.reduced_current_projection = Some(replacement_projection.clone());
        request.configuration_fence = retire_configuration_fence(&replacement_projection);
        request.parent_action_signature = retire_parent_signature(&replacement_projection);
        request.removal_mode = Some(RemoveReplicaMode::Force);
        request.input_signature = request.signature();
        assert_eq!(
            target.execute(request).await.unwrap().state,
            PeerStageState::Accepted
        );

        let demote = target.runtime_rx.recv().await.unwrap();
        assert!(matches!(
            demote.effect,
            RuntimeEffect::ChangeRole {
                role: Role::None,
                ..
            }
        ));
        target
            .status_tx
            .send_modify(|status| status.role = Role::None);
        demote.reply.send(Ok(RuntimeEffectResult::Unit)).unwrap();
        let close = target.runtime_rx.recv().await.unwrap();
        assert!(matches!(close.effect, RuntimeEffect::Close));
        target.status_tx.send_modify(|status| {
            status.role = Role::None;
            status.partition_state = None;
        });
        close.reply.send(Ok(RuntimeEffectResult::Unit)).unwrap();
        target
            .wait_for_terminal("retire-old-incarnation", PeerStageState::Completed)
            .await;
    }

    #[tokio::test]
    async fn retire_runtime_failure_and_deadlines_are_typed_and_bounded() {
        let sender = SenderStatusServer::start(sender_status(reduced_current_projection())).await;
        let mut target = RunningPeerAgent::start(
            Role::ActiveSecondary,
            true,
            ManualRemoveReplicaClock::new(100),
        );
        let request = retire_request(&target, &sender, 0, "retire-runtime-failure");
        assert_eq!(
            target.execute(request).await.unwrap().state,
            PeerStageState::Accepted
        );
        let effect = target.runtime_rx.recv().await.unwrap();
        effect
            .reply
            .send(Err(KubericError::Internal("demote failed".into())))
            .unwrap();
        let failed = target
            .wait_for_terminal("retire-runtime-failure", PeerStageState::Failed)
            .await;
        assert!(failed.error.unwrap().contains("demote failed"));

        let expired_clock = ManualRemoveReplicaClock::new(160);
        let expired_target = RunningPeerAgent::start(Role::ActiveSecondary, true, expired_clock);
        let expired = retire_request(&expired_target, &sender, 0, "retire-expired");
        let stale = expired_target.execute(expired).await.unwrap();
        assert_eq!(stale.state, PeerStageState::Stale);
        assert!(
            expired_target
                .status()
                .await
                .retained_terminal_actions
                .is_empty()
        );

        let delayed_sender = SenderStatusServer::start_with_delay(
            sender_status(reduced_current_projection()),
            Duration::from_secs(2),
        )
        .await;
        let mut bounded_target = RunningPeerAgent::start(
            Role::ActiveSecondary,
            true,
            ManualRemoveReplicaClock::new(100),
        );
        let mut bounded = retire_request(&bounded_target, &delayed_sender, 0, "retire-budget");
        bounded.retirement_expiry_unix_seconds = Some(101);
        bounded.input_signature = bounded.signature();
        let started = tokio::time::Instant::now();
        assert_eq!(
            bounded_target.execute(bounded).await.unwrap().state,
            PeerStageState::Accepted
        );
        bounded_target
            .wait_for_terminal("retire-budget", PeerStageState::Stale)
            .await;
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(bounded_target.runtime_rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn retire_restart_boundaries_recover_from_live_target_postconditions() {
        let sender = SenderStatusServer::start(sender_status(reduced_current_projection())).await;

        let mut after_demote =
            RunningPeerAgent::start(Role::None, true, ManualRemoveReplicaClock::new(100));
        let request = retire_request(&after_demote, &sender, 0, "restart-after-demote");
        assert_eq!(
            after_demote.execute(request).await.unwrap().state,
            PeerStageState::Accepted
        );
        let effect = after_demote.runtime_rx.recv().await.unwrap();
        assert!(matches!(effect.effect, RuntimeEffect::Close));
        after_demote.status_tx.send_modify(|status| {
            status.role = Role::None;
            status.partition_state = None;
        });
        effect.reply.send(Ok(RuntimeEffectResult::Unit)).unwrap();
        after_demote
            .wait_for_terminal("restart-after-demote", PeerStageState::Completed)
            .await;

        for message_id in ["restart-after-close", "restart-before-terminal-record"] {
            let mut closed =
                RunningPeerAgent::start(Role::None, false, ManualRemoveReplicaClock::new(100));
            let request = retire_request(&closed, &sender, 0, message_id);
            assert_eq!(
                closed.execute(request).await.unwrap().state,
                PeerStageState::Accepted
            );
            closed
                .wait_for_terminal(message_id, PeerStageState::Completed)
                .await;
            assert!(closed.runtime_rx.try_recv().is_err());
        }
    }

    #[tokio::test]
    async fn retire_terminal_status_retention_is_bounded() {
        let sender = SenderStatusServer::start(sender_status(reduced_current_projection())).await;
        let mut target =
            RunningPeerAgent::start(Role::None, false, ManualRemoveReplicaClock::new(100));

        for index in 0..=PEER_TERMINAL_RETENTION {
            let status = target.status().await;
            let message_id = format!("retire-retained-{index}");
            let request =
                retire_request(&target, &sender, status.peer_control_version, &message_id);
            assert_eq!(
                target.execute(request).await.unwrap().state,
                PeerStageState::Accepted
            );
            target
                .wait_for_terminal(&message_id, PeerStageState::Completed)
                .await;
        }

        let status = target.status().await;
        assert_eq!(status.peer_control_version, 17);
        assert_eq!(
            status.retained_terminal_actions.len(),
            PEER_TERMINAL_RETENTION
        );
        assert_eq!(
            status.retained_terminal_actions.first().unwrap().message_id,
            "retire-retained-1"
        );
        assert!(target.runtime_rx.try_recv().is_err());
    }

    #[test]
    fn delayed_old_generation_retire_peer_completion_cannot_enter_status() {
        let (mut agent, _runtime_rx) = test_agent(Epoch::new(4, 9), false);
        let request = peer_request(&agent, "current-peer");
        let observation = PeerStageObservation {
            protocol_version: request.protocol_version,
            operation_kind: request.operation_kind,
            stage_semantic_version: request.stage_semantic_version,
            message_id: request.message_id,
            input_signature: request.input_signature,
            stage: request.stage,
            state: PeerStageState::InProgress,
            target_agent_generation: agent.generation.clone(),
            target_peer_control_version: 1,
            error: None,
        };
        let current_execution = RuntimeEffectExecutionId {
            generation: agent.generation.clone(),
            sequence: 2,
        };
        agent.active_peer = Some(ActivePeerStage {
            execution_id: current_execution.clone(),
            observation: observation.clone(),
        });
        agent.handle_peer_completion(PeerStageCompletion {
            execution_id: RuntimeEffectExecutionId {
                generation: generation('f'),
                sequence: current_execution.sequence,
            },
            state: PeerStageState::Completed,
            error: None,
        });
        assert_eq!(agent.active_peer.as_ref().unwrap().observation, observation);
        assert!(agent.peer_terminals.is_empty());
    }

    #[tokio::test]
    async fn peer_duplicate_conflict_and_version_fences_precede_effects() {
        let (mut agent, mut runtime_rx) = test_agent(Epoch::new(1, 2), false);
        let request = peer_request(&agent, "peer");
        let duplicate = request.clone();
        let accepted = accept_peer(&mut agent, request).await.unwrap();
        assert_eq!(accepted.state, PeerStageState::Accepted);
        let replay = accept_peer(&mut agent, duplicate.clone()).await.unwrap();
        assert_eq!(replay.state, PeerStageState::InProgress);
        assert_eq!(replay.message_id, accepted.message_id);
        assert!(runtime_rx.try_recv().is_err());

        let mut conflict = duplicate.clone();
        conflict.attempt_id = "attempt-2".to_string();
        conflict.input_signature = conflict.signature();
        let conflict = accept_peer(&mut agent, conflict).await.unwrap();
        assert_eq!(conflict.state, PeerStageState::Conflict);

        let mut unsupported = duplicate.clone();
        unsupported.protocol_version += 1;
        assert!(matches!(
            accept_peer(&mut agent, unsupported).await,
            Err(KubericError::UnsupportedPeerProtocolVersion { .. })
        ));

        let mut remove = duplicate.clone();
        remove.operation_kind = PeerOperationKind::Remove;
        remove.input_signature = remove.signature();
        let rejected = accept_peer(&mut agent, remove).await.unwrap();
        assert_eq!(rejected.state, PeerStageState::Rejected);

        let mut unsupported_stage_semantics = duplicate;
        unsupported_stage_semantics.stage_semantic_version += 1;
        unsupported_stage_semantics.input_signature = unsupported_stage_semantics.signature();
        let rejected = accept_peer(&mut agent, unsupported_stage_semantics)
            .await
            .unwrap();
        assert_eq!(rejected.state, PeerStageState::Rejected);

        let mut other_sender = peer_request(&agent, "other-sender");
        other_sender.expected_target_peer_control_version = agent.peer_control_version;
        other_sender.sender_replica_id = 10;
        other_sender.sender_instance_id = ReplicaInstanceId::new("other-primary");
        other_sender.input_signature = other_sender.signature();
        let stale = accept_peer(&mut agent, other_sender).await.unwrap();
        assert_eq!(stale.state, PeerStageState::Stale);
    }

    #[test]
    fn peer_status_is_generation_and_identity_fenced() {
        let (agent, _runtime_rx) = test_agent(Epoch::new(1, 2), false);
        assert!(
            agent
                .peer_status(
                    agent.replica_id,
                    agent.instance_id.clone(),
                    agent.generation.clone(),
                )
                .is_ok()
        );
        assert!(
            agent
                .peer_status(
                    agent.replica_id,
                    ReplicaInstanceId::new("other"),
                    agent.generation.clone(),
                )
                .is_err()
        );
    }
}
