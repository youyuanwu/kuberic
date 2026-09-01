use std::collections::VecDeque;

use tokio::sync::{mpsc, oneshot, watch};
use tracing::{debug, info, warn};

use crate::Result;
use crate::add_replica::{
    AddReplicaCoordinatorPhase, AddReplicaProgress, AddReplicaTerminalResult, PeerAddBuildStatus,
    PeerStage, PeerStageObservation, PeerStageRequest, PeerStageState,
    REPLICA_ADD_BUILD_PEER_PROTOCOL_VERSION,
};
use crate::error::KubericError;
use crate::pod::{
    RuntimeControlSnapshot, RuntimeEffect, RuntimeEffectCommand, RuntimeEffectResult,
};
use crate::types::{
    AccessStatus, AgentControlVersion, AgentGeneration, CancellationToken,
    CorrelatedActionObservation, CorrelatedControlActionAcknowledgement,
    CorrelatedControlActionRequest, DurableActionErrorClass, DurableActionObservation,
    DurableActionResult, DurableActionState, DurableReplicaAction, FaultType, LocalFaultRecord,
    Lsn, ReplicaAgentStatus, ReplicaId, ReplicaInfo, ReplicaInstanceId, ReplicaStatusInfo,
};

pub const TERMINAL_RETENTION: usize = 16;
pub const FAULT_RETENTION: usize = 16;
const MAX_ERROR_BYTES: usize = 1024;
pub const CORRELATED_CONTROL_PROTOCOL_VERSION: u32 = 2;

/// Transport-facing commands accepted by the pod-local replica agent.
pub enum AgentCommand {
    ExecuteCorrelatedControlAction {
        request: Box<CorrelatedControlActionRequest>,
        reply: oneshot::Sender<Result<CorrelatedControlActionAcknowledgement>>,
    },
    GetStatus {
        reply: oneshot::Sender<ReplicaStatusInfo>,
    },
    ExecuteAddBuildStage {
        request: Box<PeerStageRequest>,
        reply: oneshot::Sender<Result<PeerStageObservation>>,
    },
    GetAddBuildStatus {
        target_replica_id: ReplicaId,
        target_instance_id: ReplicaInstanceId,
        expected_generation: AgentGeneration,
        reply: oneshot::Sender<Result<PeerAddBuildStatus>>,
    },
}

struct ActiveCorrelated {
    execution_id: RuntimeEffectExecutionId,
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
}

enum CoordinatorUpdate {
    Progress(AddReplicaProgress),
    Terminal(std::result::Result<AddReplicaTerminalResult, String>),
}

struct CoordinatorEvent {
    execution_id: RuntimeEffectExecutionId,
    update: CoordinatorUpdate,
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
            peer_terminals: VecDeque::with_capacity(crate::add_replica::PEER_TERMINAL_RETENTION),
            terminals: VecDeque::with_capacity(TERMINAL_RETENTION),
            faults: VecDeque::with_capacity(FAULT_RETENTION),
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
                        Some(AgentCommand::GetAddBuildStatus {
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
                        Some(AgentCommand::ExecuteAddBuildStage { request, reply }) => {
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
            let parent_action_signature = observation.action.signature.clone();
            self.active = Some(ActiveCorrelated {
                execution_id: execution_id.clone(),
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
            let cancelled_execution_id = execution_id.clone();
            tokio::spawn(async move {
                tokio::select! {
                    _ = shutdown.cancelled() => {
                        let _ = cancelled_tx.send(CoordinatorEvent {
                            execution_id: cancelled_execution_id,
                            update: CoordinatorUpdate::Terminal(Err(
                                "add-replica coordinator cancelled by agent shutdown".to_string(),
                            )),
                        });
                    }
                    _ = run_add_replica_coordinator(
                        execution_id,
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

    fn accept_peer_stage(
        &mut self,
        request: PeerStageRequest,
        reply: oneshot::Sender<Result<PeerStageObservation>>,
    ) {
        if request.protocol_version != REPLICA_ADD_BUILD_PEER_PROTOCOL_VERSION {
            let _ = reply.send(Err(KubericError::UnsupportedPeerProtocolVersion {
                got: request.protocol_version,
            }));
            return;
        }
        if request.target_replica_id != self.replica_id
            || request.target_instance_id != self.instance_id
        {
            let _ = reply.send(Err(KubericError::PeerStageTargetMismatch(format!(
                "expected {}@{}, got {}@{}",
                self.replica_id,
                self.instance_id,
                request.target_replica_id,
                request.target_instance_id
            ))));
            return;
        }
        if request.expected_target_agent_generation != self.generation {
            let _ = reply.send(Err(KubericError::PeerStageStale(format!(
                "expected target generation {}, current {}",
                request.expected_target_agent_generation, self.generation
            ))));
            return;
        }
        let actual_signature = request.signature();
        if request.input_signature != actual_signature {
            let _ = reply.send(Err(KubericError::ActionSignatureMismatch {
                action_id: request.message_id,
            }));
            return;
        }
        if let Some(observation) = self.find_peer_stage(&request.message_id) {
            if observation.input_signature != request.input_signature {
                let _ = reply.send(Err(KubericError::PeerStageIdConflict {
                    message_id: request.message_id,
                }));
            } else {
                let _ = reply.send(Ok(observation.clone()));
            }
            return;
        }
        if request.expected_target_peer_control_version != self.peer_control_version {
            let _ = reply.send(Err(KubericError::PeerStageStale(format!(
                "expected peer control version {}, current {}",
                request.expected_target_peer_control_version, self.peer_control_version
            ))));
            return;
        }
        if let Some(fence) = &self.peer_sender_fence {
            if request.epoch < fence.epoch
                || (request.epoch == fence.epoch
                    && (request.sender_replica_id != fence.sender_replica_id
                        || request.sender_instance_id != fence.sender_instance_id
                        || request.sender_agent_generation != fence.sender_generation))
            {
                let _ = reply.send(Err(KubericError::PeerStageStale(
                    "peer sender conflicts with the pinned primary fence".to_string(),
                )));
                return;
            }
        }
        if self.active.is_some() || self.active_peer.is_some() {
            let _ = reply.send(Err(KubericError::AgentBusy));
            return;
        }
        if request.epoch < self.runtime_status_rx.borrow().epoch {
            let observation = PeerStageObservation {
                protocol_version: REPLICA_ADD_BUILD_PEER_PROTOCOL_VERSION,
                message_id: request.message_id,
                input_signature: request.input_signature,
                stage: request.stage,
                state: PeerStageState::Stale,
                target_agent_generation: self.generation.clone(),
                target_peer_control_version: self.peer_control_version,
                error: Some("peer stage epoch is older than target runtime epoch".to_string()),
            };
            let _ = reply.send(Ok(observation));
            return;
        }

        self.peer_control_version = self.peer_control_version.saturating_add(1);
        self.peer_sender_fence = Some(PeerSenderFence {
            epoch: request.epoch,
            sender_replica_id: request.sender_replica_id,
            sender_instance_id: request.sender_instance_id.clone(),
            sender_generation: request.sender_agent_generation.clone(),
        });
        let execution_id = self.next_execution_id();
        let observation = PeerStageObservation {
            protocol_version: REPLICA_ADD_BUILD_PEER_PROTOCOL_VERSION,
            message_id: request.message_id.clone(),
            input_signature: request.input_signature.clone(),
            stage: request.stage,
            state: PeerStageState::Accepted,
            target_agent_generation: self.generation.clone(),
            target_peer_control_version: self.peer_control_version,
            error: None,
        };
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
        let shutdown = self.shutdown.child_token();
        tokio::spawn(async move {
            let result = tokio::select! {
                _ = shutdown.cancelled() => {
                    Err(PeerStageRunError::Failed(
                        "peer stage cancelled by agent shutdown".to_string(),
                    ))
                }
                result = run_peer_stage(request, runtime_tx, runtime_status_rx) => result,
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

    fn peer_status(
        &self,
        target_replica_id: ReplicaId,
        target_instance_id: ReplicaInstanceId,
        expected_generation: AgentGeneration,
    ) -> Result<PeerAddBuildStatus> {
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
        Ok(PeerAddBuildStatus {
            protocol_version: REPLICA_ADD_BUILD_PEER_PROTOCOL_VERSION,
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
        if self.peer_terminals.len() == crate::add_replica::PEER_TERMINAL_RETENTION {
            self.peer_terminals.pop_front();
        }
        self.peer_terminals.push_back(active.observation);
    }

    fn handle_coordinator_event(&mut self, event: CoordinatorEvent) {
        let Some(active) = self.active.as_mut() else {
            warn!("discarding add coordinator event without active action");
            return;
        };
        if active.execution_id != event.execution_id {
            warn!("discarding stale add coordinator event");
            return;
        }
        match event.update {
            CoordinatorUpdate::Progress(progress) => {
                active.observation.action.add_replica_progress = Some(progress);
            }
            CoordinatorUpdate::Terminal(result) => {
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
            observation,
            reply,
        });
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
                add_build_peer_protocol_version:
                    crate::add_replica::REPLICA_ADD_BUILD_PEER_PROTOCOL_VERSION,
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
) -> std::result::Result<(), PeerStageRunError> {
    match request.stage {
        PeerStage::Prepare => {
            authorize_peer_sender(&request, false).await?;
            if runtime_status_rx.borrow().partition_state.is_none() {
                send_runtime_effect(
                    &runtime_tx,
                    RuntimeEffect::Open {
                        mode: crate::types::OpenMode::New,
                    },
                )
                .await?;
            }

            authorize_peer_sender(&request, false).await?;
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

            authorize_peer_sender(&request, false).await?;
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
            authorize_peer_sender(&request, false).await?;
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
            authorize_peer_sender(&request, true).await?;
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
            authorize_peer_sender(&request, true).await?;
            if runtime_status_rx.borrow().partition_state.is_some() {
                send_runtime_effect(&runtime_tx, RuntimeEffect::Close).await?;
            }
            Ok(())
        }
    }
}

async fn authorize_peer_sender(
    request: &PeerStageRequest,
    cleanup: bool,
) -> std::result::Result<(), PeerStageRunError> {
    use crate::driver::ReplicaHandle;

    let handle = crate::grpc::handle::GrpcReplicaHandle::connect(
        request.sender_replica_id,
        request.sender_instance_id.clone(),
        request.sender_control_address.clone(),
        "http://unused".to_string(),
    )
    .await
    .map_err(|error| PeerStageRunError::Stale(error.to_string()))?;
    let status = tokio::time::timeout(std::time::Duration::from_secs(5), handle.get_status())
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

async fn run_add_replica_coordinator(
    execution_id: RuntimeEffectExecutionId,
    intent: crate::add_replica::AddReplicaIntent,
    parent_action_id: String,
    parent_action_signature: String,
    runtime_tx: mpsc::Sender<RuntimeEffectCommand>,
    mut runtime_status_rx: watch::Receiver<RuntimeControlSnapshot>,
    coordinator_tx: mpsc::UnboundedSender<CoordinatorEvent>,
) {
    let result = run_add_replica_coordinator_inner(
        &execution_id,
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
                    &execution_id,
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
                            execution_id: execution_id.clone(),
                            update: CoordinatorUpdate::Progress(AddReplicaProgress {
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
        execution_id,
        update: CoordinatorUpdate::Terminal(terminal),
    });
}

async fn run_add_replica_coordinator_inner(
    execution_id: &RuntimeEffectExecutionId,
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
            execution_id,
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
            execution_id,
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
        execution_id,
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
        execution_id,
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
        execution_id,
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
            execution_id,
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
            execution_id,
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
            execution_id,
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
        execution_id,
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
                PeerStageState::Failed | PeerStageState::Stale => {
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
            protocol_version: REPLICA_ADD_BUILD_PEER_PROTOCOL_VERSION,
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
            PeerStageState::Failed | PeerStageState::Stale => {
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
    execution_id: &RuntimeEffectExecutionId,
    intent: &crate::add_replica::AddReplicaIntent,
    parent_action_id: &str,
    parent_action_signature: &str,
    runtime_tx: &mpsc::Sender<RuntimeEffectCommand>,
    runtime_status_rx: &mut watch::Receiver<RuntimeControlSnapshot>,
    coordinator_tx: &mpsc::UnboundedSender<CoordinatorEvent>,
) -> std::result::Result<(), String> {
    send_add_progress(
        coordinator_tx,
        execution_id,
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
    execution_id: &RuntimeEffectExecutionId,
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
        execution_id: execution_id.clone(),
        update: CoordinatorUpdate::Progress(AddReplicaProgress {
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
        AgentCommand::ExecuteAddBuildStage { reply, .. } => {
            let _ = reply.send(Err(error));
        }
        AgentCommand::GetAddBuildStatus { reply, .. } => {
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
    use std::sync::Arc;

    use crate::handles::PartitionState;
    use crate::types::{Epoch, FaultType, Role};

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
            protocol_version: REPLICA_ADD_BUILD_PEER_PROTOCOL_VERSION,
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
        assert!(matches!(
            accept_peer(&mut agent, conflict).await,
            Err(KubericError::PeerStageIdConflict { .. })
        ));

        let mut unsupported = duplicate;
        unsupported.protocol_version += 1;
        assert!(matches!(
            accept_peer(&mut agent, unsupported).await,
            Err(KubericError::UnsupportedPeerProtocolVersion { .. })
        ));

        let mut other_sender = peer_request(&agent, "other-sender");
        other_sender.expected_target_peer_control_version = agent.peer_control_version;
        other_sender.sender_replica_id = 10;
        other_sender.sender_instance_id = ReplicaInstanceId::new("other-primary");
        other_sender.input_signature = other_sender.signature();
        assert!(matches!(
            accept_peer(&mut agent, other_sender).await,
            Err(KubericError::PeerStageStale(_))
        ));
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
