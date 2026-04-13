use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{mpsc, oneshot};
use tracing::warn;

use crate::error::{KubericError, Result};
use crate::events::{LifecycleEvent, ReplicateRequest, ReplicatorControlEvent};
use crate::handles::{PartitionHandle, PartitionState, StateReplicatorHandle};
use crate::types::{AccessStatus, CancellationToken, Epoch, FaultType, OpenMode, Role};

const DEFAULT_REPLY_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_CONTROL_BUFFER: usize = 16;
const DEFAULT_DATA_BUFFER: usize = 256;
const DEFAULT_SERVICE_BUFFER: usize = 16;
const DEFAULT_FAULT_BUFFER: usize = 4;

/// The kuberic runtime. Wires the replicator actor, manages access status,
/// enforces promotion/demotion ordering, and delivers events to the user.
pub struct KubericRuntime {
    lifecycle_tx: mpsc::Sender<LifecycleEvent>,
    control_tx: mpsc::Sender<ReplicatorControlEvent>,
    #[allow(dead_code)]
    data_tx: mpsc::Sender<ReplicateRequest>,
    state: Arc<PartitionState>,
    #[allow(dead_code)]
    fault_rx: mpsc::Receiver<FaultType>,
    reply_timeout: Duration,
    token: CancellationToken,
}

/// Builder for constructing a KubericRuntime.
pub struct KubericRuntimeBuilder {
    reply_timeout: Duration,
    control_buffer: usize,
    data_buffer: usize,
    service_buffer: usize,
}

impl Default for KubericRuntimeBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl KubericRuntimeBuilder {
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
        let (control_tx, control_rx) = mpsc::channel(self.control_buffer);
        let (data_tx, data_rx) = mpsc::channel(self.data_buffer);
        let state = Arc::new(PartitionState::new());
        let (lifecycle_tx, lifecycle_rx) = mpsc::channel(self.service_buffer);
        let (fault_tx, fault_rx) = mpsc::channel(DEFAULT_FAULT_BUFFER);

        let partition = Arc::new(PartitionHandle::new(state.clone(), fault_tx));
        let replicator_handle = StateReplicatorHandle::new(data_tx.clone(), state.clone());

        let runtime = KubericRuntime {
            lifecycle_tx,
            control_tx,
            data_tx,
            state: state.clone(),
            fault_rx,
            reply_timeout: self.reply_timeout,
            token: CancellationToken::new(),
        };

        RuntimeBundle {
            runtime,
            replicator_control_rx: control_rx,
            replicator_data_rx: data_rx,
            lifecycle_rx,
            partition,
            replicator_handle,
            state,
        }
    }
}

/// Everything produced by the builder. The caller spawns the replicator actor
/// with `replicator_control_rx` + `replicator_data_rx`, and runs the user
/// service event loop with `lifecycle_rx`.
pub struct RuntimeBundle {
    pub runtime: KubericRuntime,
    pub replicator_control_rx: mpsc::Receiver<ReplicatorControlEvent>,
    pub replicator_data_rx: mpsc::Receiver<ReplicateRequest>,
    pub lifecycle_rx: mpsc::Receiver<LifecycleEvent>,
    pub partition: Arc<PartitionHandle>,
    pub replicator_handle: StateReplicatorHandle,
    pub state: Arc<PartitionState>,
}

impl KubericRuntime {
    pub fn builder() -> KubericRuntimeBuilder {
        KubericRuntimeBuilder::new()
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
            .map_err(|_| KubericError::Closed)?;

        match tokio::time::timeout(self.reply_timeout, rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(KubericError::Closed),
            Err(_) => {
                warn!("replicator control event timed out");
                Err(KubericError::Internal("replicator reply timeout".into()))
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
            Ok(Err(_)) => return Err(KubericError::Closed),
            Err(_) => {
                warn!("lifecycle event channel send timed out (backpressure)");
                return Err(KubericError::Internal(
                    "lifecycle event send timeout".into(),
                ));
            }
        }

        match tokio::time::timeout(self.reply_timeout, rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(KubericError::Closed),
            Err(_) => {
                warn!("lifecycle event reply timed out");
                Err(KubericError::Internal("lifecycle reply timeout".into()))
            }
        }
    }

    // -----------------------------------------------------------------------
    // Lifecycle operations (called by the operator or test harness)
    // -----------------------------------------------------------------------

    /// Open the replicator. In the new API, the user creates the replicator
    /// in the Open callback. This legacy runtime pre-creates everything and
    /// sends OpenContext for the user to create the replicator.
    ///
    /// For backward compatibility with tests that use RuntimeBundle's
    /// pre-created channels, this method sends a minimal OpenContext and
    /// the test's Open handler returns a ReplicatorHandle that wraps
    /// the pre-created control_tx.
    pub async fn open(&self, mode: OpenMode) -> Result<()> {
        // 1. Open replicator directly (we still hold control_tx)
        self.send_control(|reply| ReplicatorControlEvent::Open { mode, reply })
            .await?;

        // 2. Deliver Open to user — they return a ReplicatorHandle
        // For KubericRuntime tests, the user returns a handle wrapping
        // the same control_tx we already have (no-op separation).
        let _handle: crate::replicator::ReplicatorHandle = self
            .send_lifecycle(|reply| LifecycleEvent::Open {
                ctx: crate::replicator::OpenContext {
                    replica_id: 0, // tests don't use this
                    open_mode: mode,
                    data_bind: "127.0.0.1:0".to_string(),
                    token: self.token.child_token(),
                    fault_tx: mpsc::channel(4).0,
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
        let bundle = KubericRuntime::builder()
            .reply_timeout(Duration::from_secs(5))
            .build();

        let state = bundle.state.clone();
        let _partition = bundle.partition.clone();
        let runtime = bundle.runtime;
        let mut lifecycle_rx = bundle.lifecycle_rx;

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
        let user_replicator = bundle.replicator_handle;
        let user_handle = tokio::spawn(async move {
            let mut svc_replicator = None;

            while let Some(event) = lifecycle_rx.recv().await {
                match event {
                    LifecycleEvent::Open { ctx: _, reply } => {
                        // In KubericRuntime tests, the replicator is pre-created.
                        // We return a dummy handle since the runtime already opened
                        // the replicator directly.
                        svc_replicator = Some(user_replicator.clone());
                        let dummy_handle = crate::replicator::ReplicatorHandle::new(
                            mpsc::channel(1).0,
                            Arc::new(PartitionState::new()),
                            String::new(),
                            CancellationToken::new(),
                        );
                        let _ = reply.send(Ok(dummy_handle));
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
        runtime.open(OpenMode::New).await.unwrap();

        // Status: not primary yet
        assert_eq!(state.read_status(), AccessStatus::NotPrimary);
        assert_eq!(state.write_status(), AccessStatus::NotPrimary);

        // 2. ChangeRole to IdleSecondary
        runtime
            .change_role(Epoch::new(0, 1), Role::IdleSecondary, Role::Unknown)
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
        let bundle = KubericRuntime::builder()
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
        let _user_handle = tokio::spawn(async move {
            while let Some(event) = lifecycle_rx.recv().await {
                match event {
                    LifecycleEvent::Open { reply, .. } => {
                        let dummy_handle = crate::replicator::ReplicatorHandle::new(
                            mpsc::channel(1).0,
                            Arc::new(PartitionState::new()),
                            String::new(),
                            CancellationToken::new(),
                        );
                        let _ = reply.send(Ok(dummy_handle));
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
        runtime.open(OpenMode::New).await.unwrap();

        // Try to replicate — should fail fast
        let repl = StateReplicatorHandle::new(runtime.data_tx.clone(), state.clone());
        let result = repl
            .replicate(bytes::Bytes::from("should-fail"), CancellationToken::new())
            .await;

        assert!(matches!(result, Err(KubericError::NotPrimary)));

        runtime.close().await.unwrap();
    }

    /// Test abort path.
    #[tokio::test]
    async fn test_runtime_abort() {
        let bundle = KubericRuntime::builder()
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
