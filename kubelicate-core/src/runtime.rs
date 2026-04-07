use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{mpsc, oneshot};
use tracing::warn;

use crate::error::{KubelicateError, Result};
use crate::events::{
    LifecycleEvent, ReplicateRequest, ReplicatorChannels, ReplicatorControlEvent, ServiceContext,
    StateProviderEvent,
};
use crate::handles::{PartitionHandle, PartitionState, StateReplicatorHandle};
use crate::types::{AccessStatus, CancellationToken, Epoch, FaultType, OpenMode, Role};

const DEFAULT_REPLY_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_CONTROL_BUFFER: usize = 16;
const DEFAULT_DATA_BUFFER: usize = 256;
const DEFAULT_SERVICE_BUFFER: usize = 16;
const DEFAULT_FAULT_BUFFER: usize = 4;

/// The kubelicate runtime. Wires the replicator actor, manages access status,
/// enforces promotion/demotion ordering, and delivers events to the user.
pub struct KubelicateRuntime {
    lifecycle_tx: mpsc::Sender<LifecycleEvent>,
    state_provider_tx: mpsc::Sender<StateProviderEvent>,
    control_tx: mpsc::Sender<ReplicatorControlEvent>,
    #[allow(dead_code)]
    data_tx: mpsc::Sender<ReplicateRequest>,
    state: Arc<PartitionState>,
    #[allow(dead_code)]
    fault_rx: mpsc::Receiver<FaultType>,
    reply_timeout: Duration,
    token: CancellationToken,
}

/// Builder for constructing a KubelicateRuntime.
pub struct KubelicateRuntimeBuilder {
    reply_timeout: Duration,
    control_buffer: usize,
    data_buffer: usize,
    service_buffer: usize,
}

impl Default for KubelicateRuntimeBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl KubelicateRuntimeBuilder {
    pub fn new() -> Self {
        Self {
            reply_timeout: DEFAULT_REPLY_TIMEOUT,
            control_buffer: DEFAULT_CONTROL_BUFFER,
            data_buffer: DEFAULT_DATA_BUFFER,
            service_buffer: DEFAULT_SERVICE_BUFFER,
        }
    }

    pub fn reply_timeout(mut self, timeout: Duration) -> Self {
        self.reply_timeout = timeout;
        self
    }

    /// Build the runtime, returning it along with the channels needed to
    /// spawn the replicator actor and the user service event receivers.
    pub fn build(self) -> RuntimeBundle {
        let channels = ReplicatorChannels::new(self.control_buffer, self.data_buffer);
        let state = Arc::new(PartitionState::new());
        let (lifecycle_tx, lifecycle_rx) = mpsc::channel(self.service_buffer);
        let (state_provider_tx, state_provider_rx) = mpsc::channel(self.service_buffer);
        let (fault_tx, fault_rx) = mpsc::channel(DEFAULT_FAULT_BUFFER);

        let partition = Arc::new(PartitionHandle::new(state.clone(), fault_tx));
        let replicator_handle = StateReplicatorHandle::new(channels.data_tx.clone(), state.clone());

        let runtime = KubelicateRuntime {
            lifecycle_tx,
            state_provider_tx,
            control_tx: channels.control_tx,
            data_tx: channels.data_tx,
            state: state.clone(),
            fault_rx,
            reply_timeout: self.reply_timeout,
            token: CancellationToken::new(),
        };

        RuntimeBundle {
            runtime,
            replicator_control_rx: channels.control_rx,
            replicator_data_rx: channels.data_rx,
            lifecycle_rx,
            state_provider_rx,
            partition,
            replicator_handle,
            state,
        }
    }
}

/// Everything produced by the builder. The caller spawns the replicator actor
/// with `replicator_control_rx` + `replicator_data_rx`, and runs the user
/// service event loop with `lifecycle_rx` + `state_provider_rx`.
pub struct RuntimeBundle {
    pub runtime: KubelicateRuntime,
    pub replicator_control_rx: mpsc::Receiver<ReplicatorControlEvent>,
    pub replicator_data_rx: mpsc::Receiver<ReplicateRequest>,
    pub lifecycle_rx: mpsc::Receiver<LifecycleEvent>,
    pub state_provider_rx: mpsc::Receiver<StateProviderEvent>,
    pub partition: Arc<PartitionHandle>,
    pub replicator_handle: StateReplicatorHandle,
    pub state: Arc<PartitionState>,
}

impl KubelicateRuntime {
    pub fn builder() -> KubelicateRuntimeBuilder {
        KubelicateRuntimeBuilder::new()
    }

    /// Send a control event to the replicator and await the reply with timeout.
    async fn send_control<T>(
        &self,
        make_event: impl FnOnce(oneshot::Sender<Result<T>>) -> ReplicatorControlEvent,
    ) -> Result<T> {
        let (tx, rx) = oneshot::channel();
        self.control_tx
            .send(make_event(tx))
            .await
            .map_err(|_| KubelicateError::Closed)?;

        match tokio::time::timeout(self.reply_timeout, rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(KubelicateError::Closed),
            Err(_) => {
                warn!("replicator control event timed out");
                Err(KubelicateError::Internal("replicator reply timeout".into()))
            }
        }
    }

    /// Send a lifecycle event to the user and await the reply with timeout.
    async fn send_lifecycle<T>(
        &self,
        make_event: impl FnOnce(oneshot::Sender<Result<T>>) -> LifecycleEvent,
    ) -> Result<T> {
        let (tx, rx) = oneshot::channel();
        let event = make_event(tx);

        match tokio::time::timeout(self.reply_timeout, self.lifecycle_tx.send(event)).await {
            Ok(Ok(())) => {}
            Ok(Err(_)) => return Err(KubelicateError::Closed),
            Err(_) => {
                warn!("lifecycle event channel send timed out (backpressure)");
                return Err(KubelicateError::Internal(
                    "lifecycle event send timeout".into(),
                ));
            }
        }

        match tokio::time::timeout(self.reply_timeout, rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(KubelicateError::Closed),
            Err(_) => {
                warn!("lifecycle event reply timed out");
                Err(KubelicateError::Internal("lifecycle reply timeout".into()))
            }
        }
    }

    /// Send a state provider event to the user and await the reply with timeout.
    #[allow(dead_code)]
    async fn send_state_provider<T>(
        &self,
        make_event: impl FnOnce(oneshot::Sender<Result<T>>) -> StateProviderEvent,
    ) -> Result<T> {
        let (tx, rx) = oneshot::channel();
        let event = make_event(tx);

        match tokio::time::timeout(self.reply_timeout, self.state_provider_tx.send(event)).await {
            Ok(Ok(())) => {}
            Ok(Err(_)) => return Err(KubelicateError::Closed),
            Err(_) => {
                warn!("state_provider event channel send timed out");
                return Err(KubelicateError::Internal(
                    "state_provider event send timeout".into(),
                ));
            }
        }

        match tokio::time::timeout(self.reply_timeout, rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(KubelicateError::Closed),
            Err(_) => {
                warn!("state_provider event reply timed out");
                Err(KubelicateError::Internal(
                    "state_provider reply timeout".into(),
                ))
            }
        }
    }

    // -----------------------------------------------------------------------
    // Lifecycle operations (called by the operator or test harness)
    // -----------------------------------------------------------------------

    /// Open the replicator, then deliver Open event to the user service.
    pub async fn open(
        &self,
        mode: OpenMode,
        partition: Arc<PartitionHandle>,
        replicator_handle: StateReplicatorHandle,
    ) -> Result<()> {
        // 1. Open replicator
        self.send_control(|reply| ReplicatorControlEvent::Open { mode, reply })
            .await?;

        // 2. Deliver Open to user via lifecycle channel
        self.send_lifecycle(|reply| LifecycleEvent::Open {
            ctx: ServiceContext {
                partition,
                replicator: replicator_handle,
                copy_stream: None,
                replication_stream: None,
                token: self.token.child_token(),
            },
            reply,
        })
        .await?;

        Ok(())
    }

    /// Change role with correct ordering:
    /// - Promotion (to Primary): replicator first, then user
    /// - Demotion (from Primary): user first, then replicator
    pub async fn change_role(&self, epoch: Epoch, new_role: Role, old_role: Role) -> Result<()> {
        let is_promotion = new_role == Role::Primary
            || (new_role == Role::ActiveSecondary && old_role == Role::IdleSecondary);

        if is_promotion {
            // Promotion: replicator sets up replication BEFORE user starts work
            self.send_control(|reply| ReplicatorControlEvent::ChangeRole {
                epoch,
                role: new_role,
                reply,
            })
            .await?;

            self.set_status_for_role(new_role);

            let _addr: String = self
                .send_lifecycle(|reply| LifecycleEvent::ChangeRole { new_role, reply })
                .await?;
        } else {
            // Demotion: user stops work BEFORE replicator tears down
            self.set_status_for_role(new_role);

            let _addr: String = self
                .send_lifecycle(|reply| LifecycleEvent::ChangeRole { new_role, reply })
                .await?;

            self.send_control(|reply| ReplicatorControlEvent::ChangeRole {
                epoch,
                role: new_role,
                reply,
            })
            .await?;
        }

        Ok(())
    }

    /// Close: user first, then replicator.
    pub async fn close(&self) -> Result<()> {
        self.state
            .set_read_status(AccessStatus::ReconfigurationPending);
        self.state
            .set_write_status(AccessStatus::ReconfigurationPending);

        // 1. Close user service
        let _ = self
            .send_lifecycle(|reply| LifecycleEvent::Close { reply })
            .await;

        // 2. Close replicator
        let _ = self
            .send_control(|reply| ReplicatorControlEvent::Close { reply })
            .await;

        self.state.set_read_status(AccessStatus::NotPrimary);
        self.state.set_write_status(AccessStatus::NotPrimary);

        self.token.cancel();
        Ok(())
    }

    /// Abort: fire-and-forget to both.
    pub fn abort(&self) {
        self.state.set_read_status(AccessStatus::NotPrimary);
        self.state.set_write_status(AccessStatus::NotPrimary);

        let _ = self.lifecycle_tx.try_send(LifecycleEvent::Abort);
        let _ = self.control_tx.try_send(ReplicatorControlEvent::Abort);

        self.token.cancel();
    }

    // -----------------------------------------------------------------------
    // Replicator-only control operations (no lifecycle event counterpart)
    // -----------------------------------------------------------------------

    pub async fn update_epoch(&self, epoch: Epoch) -> Result<()> {
        self.send_control(|reply| ReplicatorControlEvent::UpdateEpoch { epoch, reply })
            .await
    }

    pub async fn update_catch_up_configuration(
        &self,
        current: crate::types::ReplicaSetConfig,
        previous: crate::types::ReplicaSetConfig,
    ) -> Result<()> {
        self.send_control(|reply| ReplicatorControlEvent::UpdateCatchUpConfiguration {
            current,
            previous,
            reply,
        })
        .await
    }

    pub async fn update_current_configuration(
        &self,
        current: crate::types::ReplicaSetConfig,
    ) -> Result<()> {
        self.send_control(|reply| ReplicatorControlEvent::UpdateCurrentConfiguration {
            current,
            reply,
        })
        .await
    }

    pub async fn wait_for_catch_up_quorum(
        &self,
        mode: crate::types::ReplicaSetQuorumMode,
    ) -> Result<()> {
        self.send_control(|reply| ReplicatorControlEvent::WaitForCatchUpQuorum { mode, reply })
            .await
    }

    pub async fn build_replica(&self, replica: crate::types::ReplicaInfo) -> Result<()> {
        self.send_control(|reply| ReplicatorControlEvent::BuildReplica { replica, reply })
            .await
    }

    pub async fn remove_replica(&self, replica_id: crate::types::ReplicaId) -> Result<()> {
        self.send_control(|reply| ReplicatorControlEvent::RemoveReplica { replica_id, reply })
            .await
    }

    pub async fn on_data_loss(&self) -> Result<crate::types::DataLossAction> {
        self.send_control(|reply| ReplicatorControlEvent::OnDataLoss { reply })
            .await
    }

    // -----------------------------------------------------------------------
    // Status queries (lock-free reads from PartitionState)
    // -----------------------------------------------------------------------

    pub fn read_status(&self) -> AccessStatus {
        self.state.read_status()
    }

    pub fn write_status(&self) -> AccessStatus {
        self.state.write_status()
    }

    pub fn current_progress(&self) -> crate::types::Lsn {
        self.state.current_progress()
    }

    pub fn catch_up_capability(&self) -> crate::types::Lsn {
        self.state.catch_up_capability()
    }

    pub fn committed_lsn(&self) -> crate::types::Lsn {
        self.state.committed_lsn()
    }

    // -----------------------------------------------------------------------
    // Internal
    // -----------------------------------------------------------------------

    fn set_status_for_role(&self, role: Role) {
        match role {
            Role::Primary => {
                self.state.set_read_status(AccessStatus::Granted);
                self.state.set_write_status(AccessStatus::Granted);
            }
            _ => {
                self.state.set_read_status(AccessStatus::NotPrimary);
                self.state.set_write_status(AccessStatus::NotPrimary);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::LifecycleEvent;
    use crate::noop::NoopReplicator;
    use crate::types::AccessStatus;

    /// Full lifecycle integration test with NoopReplicator.
    /// Exercises: open → promote → replicate → demote → close.
    #[tokio::test]
    async fn test_runtime_full_lifecycle() {
        let bundle = KubelicateRuntime::builder()
            .reply_timeout(Duration::from_secs(5))
            .build();

        let state = bundle.state.clone();
        let partition = bundle.partition.clone();
        let _replicator_handle = bundle.replicator_handle;
        let runtime = bundle.runtime;
        let mut lifecycle_rx = bundle.lifecycle_rx;
        let mut _state_provider_rx = bundle.state_provider_rx;

        // Spawn the noop replicator actor
        let repl_state = state.clone();
        let repl_handle = tokio::spawn(async move {
            NoopReplicator::run(
                bundle.replicator_control_rx,
                bundle.replicator_data_rx,
                repl_state,
            )
            .await;
        });

        // Spawn the user lifecycle event loop
        let user_handle = tokio::spawn(async move {
            let mut _svc_partition = None;
            let mut svc_replicator = None;

            while let Some(event) = lifecycle_rx.recv().await {
                match event {
                    LifecycleEvent::Open { ctx, reply } => {
                        _svc_partition = Some(ctx.partition);
                        svc_replicator = Some(ctx.replicator);
                        let _ = reply.send(Ok(()));
                    }
                    LifecycleEvent::ChangeRole { new_role, reply } => {
                        if new_role == Role::Primary {
                            // Verify we can replicate as primary
                            let repl = svc_replicator.as_ref().unwrap();
                            let lsn = repl
                                .replicate(
                                    bytes::Bytes::from("from-user-handler"),
                                    CancellationToken::new(),
                                )
                                .await
                                .unwrap();
                            assert!(lsn > 0);
                        }
                        let _ = reply.send(Ok(String::new()));
                    }
                    LifecycleEvent::Close { reply } => {
                        let _ = reply.send(Ok(()));
                        break;
                    }
                    LifecycleEvent::Abort => break,
                }
            }
        });

        // --- Drive lifecycle as the operator would ---

        // 1. Open
        runtime
            .open(
                OpenMode::New,
                partition.clone(),
                StateReplicatorHandle::new(runtime.data_tx.clone(), state.clone()),
            )
            .await
            .unwrap();

        // Status: not primary yet
        assert_eq!(state.read_status(), AccessStatus::NotPrimary);
        assert_eq!(state.write_status(), AccessStatus::NotPrimary);

        // 2. ChangeRole to IdleSecondary
        runtime
            .change_role(Epoch::new(0, 1), Role::IdleSecondary, Role::None)
            .await
            .unwrap();

        assert_eq!(state.read_status(), AccessStatus::NotPrimary);

        // 3. ChangeRole to ActiveSecondary (promotion from idle)
        runtime
            .change_role(Epoch::new(0, 1), Role::ActiveSecondary, Role::IdleSecondary)
            .await
            .unwrap();

        assert_eq!(state.read_status(), AccessStatus::NotPrimary);

        // 4. ChangeRole to Primary (promotion)
        runtime
            .change_role(Epoch::new(0, 1), Role::Primary, Role::ActiveSecondary)
            .await
            .unwrap();

        assert_eq!(state.read_status(), AccessStatus::Granted);
        assert_eq!(state.write_status(), AccessStatus::Granted);

        // 5. Replicate via the partition handle (outside user event loop)
        let repl = StateReplicatorHandle::new(runtime.data_tx.clone(), state.clone());
        let lsn = repl
            .replicate(bytes::Bytes::from("from-runtime"), CancellationToken::new())
            .await
            .unwrap();
        assert!(lsn > 0);
        assert!(state.current_progress() > 0);

        // 6. ChangeRole to ActiveSecondary (demotion)
        runtime
            .change_role(Epoch::new(0, 2), Role::ActiveSecondary, Role::Primary)
            .await
            .unwrap();

        assert_eq!(state.read_status(), AccessStatus::NotPrimary);
        assert_eq!(state.write_status(), AccessStatus::NotPrimary);

        // 7. Close
        runtime.close().await.unwrap();

        // 8. Wait for actors to finish
        repl_handle.await.unwrap();
        user_handle.await.unwrap();
    }

    /// Test that replicate fails with NotPrimary when not promoted.
    #[tokio::test]
    async fn test_runtime_replicate_before_promote() {
        let bundle = KubelicateRuntime::builder()
            .reply_timeout(Duration::from_secs(5))
            .build();

        let state = bundle.state.clone();
        let runtime = bundle.runtime;

        // Spawn noop replicator
        let repl_state = state.clone();
        let _repl_handle = tokio::spawn(async move {
            NoopReplicator::run(
                bundle.replicator_control_rx,
                bundle.replicator_data_rx,
                repl_state,
            )
            .await;
        });

        // Spawn minimal user lifecycle loop
        let mut lifecycle_rx = bundle.lifecycle_rx;
        let _state_provider_rx = bundle.state_provider_rx;
        let _user_handle = tokio::spawn(async move {
            while let Some(event) = lifecycle_rx.recv().await {
                match event {
                    LifecycleEvent::Open { reply, .. } => {
                        let _ = reply.send(Ok(()));
                    }
                    LifecycleEvent::ChangeRole { reply, .. } => {
                        let _ = reply.send(Ok(String::new()));
                    }
                    LifecycleEvent::Close { reply } => {
                        let _ = reply.send(Ok(()));
                        break;
                    }
                    LifecycleEvent::Abort => break,
                }
            }
        });

        // Open but don't promote
        runtime
            .open(
                OpenMode::New,
                bundle.partition.clone(),
                StateReplicatorHandle::new(runtime.data_tx.clone(), state.clone()),
            )
            .await
            .unwrap();

        // Try to replicate — should fail fast
        let repl = StateReplicatorHandle::new(runtime.data_tx.clone(), state.clone());
        let result = repl
            .replicate(bytes::Bytes::from("should-fail"), CancellationToken::new())
            .await;

        assert!(matches!(result, Err(KubelicateError::NotPrimary)));

        runtime.close().await.unwrap();
    }

    /// Test abort path.
    #[tokio::test]
    async fn test_runtime_abort() {
        let bundle = KubelicateRuntime::builder()
            .reply_timeout(Duration::from_secs(5))
            .build();

        let state = bundle.state.clone();
        let runtime = bundle.runtime;

        let repl_state = state.clone();
        let repl_handle = tokio::spawn(async move {
            NoopReplicator::run(
                bundle.replicator_control_rx,
                bundle.replicator_data_rx,
                repl_state,
            )
            .await;
        });

        let mut lifecycle_rx = bundle.lifecycle_rx;
        let _state_provider_rx = bundle.state_provider_rx;
        let user_handle = tokio::spawn(async move {
            while let Some(event) = lifecycle_rx.recv().await {
                if let LifecycleEvent::Abort = event {
                    break;
                }
            }
        });

        // Abort without open
        runtime.abort();

        assert_eq!(state.read_status(), AccessStatus::NotPrimary);
        assert_eq!(state.write_status(), AccessStatus::NotPrimary);

        repl_handle.await.unwrap();
        user_handle.await.unwrap();
    }
}
