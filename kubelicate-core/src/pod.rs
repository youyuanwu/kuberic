use std::time::Duration;

use tokio::sync::{mpsc, oneshot};
use tonic::transport::Server;
use tracing::{info, warn};

use crate::error::{KubelicateError, Result};
use crate::events::{LifecycleEvent, ReplicatorControlEvent};
use crate::replicator::{OpenContext, ReplicatorHandle};
use crate::types::{
    AccessStatus, CancellationToken, DataLossAction, Epoch, Lsn, OpenMode, ReplicaId, ReplicaInfo,
    ReplicaSetConfig, ReplicaSetQuorumMode, Role,
};

const DEFAULT_REPLY_TIMEOUT: Duration = Duration::from_secs(30);

// ---------------------------------------------------------------------------
// RuntimeCommand — what the gRPC control server sends to the runtime
// ---------------------------------------------------------------------------

/// Commands sent by the gRPC ControlServer to the PodRuntime.
/// The runtime processes these with correct ordering (replicator + user events).
pub enum RuntimeCommand {
    Open {
        mode: OpenMode,
        reply: oneshot::Sender<Result<()>>,
    },
    Close {
        reply: oneshot::Sender<Result<()>>,
    },
    ChangeRole {
        epoch: Epoch,
        role: Role,
        reply: oneshot::Sender<Result<()>>,
    },
    UpdateEpoch {
        epoch: Epoch,
        reply: oneshot::Sender<Result<()>>,
    },
    UpdateCatchUpConfiguration {
        current: ReplicaSetConfig,
        previous: ReplicaSetConfig,
        reply: oneshot::Sender<Result<()>>,
    },
    UpdateCurrentConfiguration {
        current: ReplicaSetConfig,
        reply: oneshot::Sender<Result<()>>,
    },
    WaitForCatchUpQuorum {
        mode: ReplicaSetQuorumMode,
        reply: oneshot::Sender<Result<()>>,
    },
    BuildReplica {
        replica: ReplicaInfo,
        reply: oneshot::Sender<Result<()>>,
    },
    RemoveReplica {
        replica_id: ReplicaId,
        reply: oneshot::Sender<Result<()>>,
    },
    OnDataLoss {
        reply: oneshot::Sender<Result<DataLossAction>>,
    },
    RevokeWriteStatus {
        reply: oneshot::Sender<Result<()>>,
    },
    GetStatus {
        reply: oneshot::Sender<StatusInfo>,
    },
}

/// Status info returned by GetStatus.
pub struct StatusInfo {
    pub role: Role,
    pub epoch: Epoch,
    pub current_progress: Lsn,
    pub catch_up_capability: Lsn,
    pub committed_lsn: Lsn,
    pub healthy: bool,
}

// ---------------------------------------------------------------------------
// PodRuntime
// ---------------------------------------------------------------------------

pub struct PodRuntime {
    lifecycle_tx: mpsc::Sender<LifecycleEvent>,
    cmd_rx: mpsc::Receiver<RuntimeCommand>,
    replicator_handle: Option<ReplicatorHandle>,
    shutdown: CancellationToken,
    reply_timeout: Duration,
    role: Role,
    epoch: Epoch,
    replica_id: ReplicaId,
    data_bind: String,
}

pub struct PodRuntimeBundle {
    pub runtime: PodRuntime,
    pub lifecycle_rx: mpsc::Receiver<LifecycleEvent>,
    pub control_address: String,
}

pub struct PodRuntimeBuilder {
    replica_id: ReplicaId,
    reply_timeout: Duration,
    control_bind: String,
    data_bind: String,
}

impl PodRuntimeBuilder {
    pub fn new(replica_id: ReplicaId) -> Self {
        Self {
            replica_id,
            reply_timeout: DEFAULT_REPLY_TIMEOUT,
            control_bind: "127.0.0.1:0".to_string(),
            data_bind: "127.0.0.1:0".to_string(),
        }
    }

    pub fn reply_timeout(mut self, timeout: Duration) -> Self {
        self.reply_timeout = timeout;
        self
    }

    pub fn control_bind(mut self, addr: String) -> Self {
        self.control_bind = addr;
        self
    }

    pub fn data_bind(mut self, addr: String) -> Self {
        self.data_bind = addr;
        self
    }

    pub async fn build(self) -> Result<PodRuntimeBundle> {
        let shutdown = CancellationToken::new();
        let (lifecycle_tx, lifecycle_rx) = mpsc::channel(16);
        let (cmd_tx, cmd_rx) = mpsc::channel(16);

        // Control plane gRPC server (runtime-owned, unchanged)
        // PartitionState is not available yet — it's created by the replicator
        // at Open time. ControlServer needs to work without it initially.
        let control_server = crate::grpc::server::ControlServer::new(self.replica_id, cmd_tx);
        let control_listener = tokio::net::TcpListener::bind(&self.control_bind)
            .await
            .map_err(|e| KubelicateError::Internal(Box::new(e)))?;
        let control_addr = control_listener.local_addr().unwrap();
        let control_address = format!("http://{}", control_addr);

        let ctrl_shutdown = shutdown.child_token();
        tokio::spawn(async move {
            let _ = Server::builder()
                .add_service(
                    crate::proto::replicator_control_server::ReplicatorControlServer::new(
                        control_server,
                    ),
                )
                .serve_with_incoming_shutdown(
                    tokio_stream::wrappers::TcpListenerStream::new(control_listener),
                    ctrl_shutdown.cancelled(),
                )
                .await;
        });

        tokio::time::sleep(Duration::from_millis(50)).await;

        info!(
            replica_id = self.replica_id,
            %control_address,
            "pod runtime started (replicator deferred to Open)"
        );

        let runtime = PodRuntime {
            lifecycle_tx,
            cmd_rx,
            replicator_handle: None,
            shutdown,
            reply_timeout: self.reply_timeout,
            role: Role::None,
            epoch: Epoch::default(),
            replica_id: self.replica_id,
            data_bind: self.data_bind,
        };

        Ok(PodRuntimeBundle {
            runtime,
            lifecycle_rx,
            control_address,
        })
    }
}

impl PodRuntime {
    pub fn builder(replica_id: ReplicaId) -> PodRuntimeBuilder {
        PodRuntimeBuilder::new(replica_id)
    }

    /// Get the shutdown token. Cancelling it triggers graceful shutdown.
    pub fn shutdown_token(&self) -> CancellationToken {
        self.shutdown.clone()
    }

    /// Run the runtime command loop. Processes operator commands from the
    /// gRPC control server with correct replicator/user event ordering.
    /// Blocks until shutdown.
    pub async fn serve(mut self) {
        info!("PodRuntime serve loop started");
        while let Some(cmd) = self.cmd_rx.recv().await {
            match cmd {
                RuntimeCommand::Open { mode, reply } => {
                    let _ = reply.send(self.handle_open(mode).await);
                }
                RuntimeCommand::Close { reply } => {
                    let _ = reply.send(self.handle_close().await);
                    break;
                }
                RuntimeCommand::ChangeRole { epoch, role, reply } => {
                    let _ = reply.send(match self.require_handle() {
                        Ok(_) => self.handle_change_role(epoch, role).await,
                        Err(e) => Err(e),
                    });
                }
                RuntimeCommand::UpdateEpoch { epoch, reply } => {
                    let _ = reply.send(self.handle_update_epoch(epoch).await);
                }
                RuntimeCommand::UpdateCatchUpConfiguration {
                    current,
                    previous,
                    reply,
                } => {
                    let _ = reply.send(
                        self.send_replicator_control(|r| {
                            ReplicatorControlEvent::UpdateCatchUpConfiguration {
                                current,
                                previous,
                                reply: r,
                            }
                        })
                        .await,
                    );
                }
                RuntimeCommand::UpdateCurrentConfiguration { current, reply } => {
                    let _ = reply.send(
                        self.send_replicator_control(|r| {
                            ReplicatorControlEvent::UpdateCurrentConfiguration { current, reply: r }
                        })
                        .await,
                    );
                }
                RuntimeCommand::WaitForCatchUpQuorum { mode, reply } => {
                    let _ = reply.send(
                        self.send_replicator_control(|r| {
                            ReplicatorControlEvent::WaitForCatchUpQuorum { mode, reply: r }
                        })
                        .await,
                    );
                }
                RuntimeCommand::BuildReplica { replica, reply } => {
                    // Tell actor to buffer (same as before)
                    let _ = self
                        .send_replicator_control(|r| ReplicatorControlEvent::BuildReplica {
                            replica: replica.clone(),
                            reply: r,
                        })
                        .await;

                    // Spawn the copy protocol in background
                    if let Some(handle) = &self.replicator_handle {
                        let state_provider_tx = handle.state_provider_tx().clone();
                        let state = handle.state().clone();
                        let reply_timeout = self.reply_timeout;
                        tokio::spawn(async move {
                            let result = run_build_replica_copy(
                                replica,
                                state_provider_tx,
                                state,
                                reply_timeout,
                            )
                            .await;
                            let _ = reply.send(result);
                        });
                    }
                }
                RuntimeCommand::RemoveReplica { replica_id, reply } => {
                    let _ = reply.send(
                        self.send_replicator_control(|r| ReplicatorControlEvent::RemoveReplica {
                            replica_id,
                            reply: r,
                        })
                        .await,
                    );
                }
                RuntimeCommand::OnDataLoss { reply } => {
                    let _ = reply.send(self.handle_on_data_loss().await);
                }
                RuntimeCommand::RevokeWriteStatus { reply } => {
                    info!("revoking write status for switchover");
                    if let Some(handle) = &self.replicator_handle {
                        handle
                            .state()
                            .set_write_status(AccessStatus::ReconfigurationPending);
                    }
                    let _ = reply.send(Ok(()));
                }
                RuntimeCommand::GetStatus { reply } => {
                    let handle = self.replicator_handle.as_ref();
                    let _ = reply.send(StatusInfo {
                        role: self.role,
                        epoch: self.epoch,
                        current_progress: handle.map_or(0, |h| h.state().current_progress()),
                        catch_up_capability: handle.map_or(0, |h| h.state().catch_up_capability()),
                        committed_lsn: handle.map_or(0, |h| h.state().committed_lsn()),
                        healthy: handle.is_some(),
                    });
                }
            }
        }
        self.shutdown.cancel();
    }

    // -----------------------------------------------------------------------
    // Command handlers with correct ordering
    // -----------------------------------------------------------------------

    /// Get handle or return error for pre-Open commands.
    fn require_handle(&self) -> Result<&ReplicatorHandle> {
        self.replicator_handle
            .as_ref()
            .ok_or(KubelicateError::Internal("replicator not opened".into()))
    }

    async fn handle_open(&mut self, mode: OpenMode) -> Result<()> {
        if self.replicator_handle.is_some() {
            return Err(KubelicateError::Internal("already opened".into()));
        }

        // 1. Send OpenContext to user, receive ReplicatorHandle back
        let (fault_tx, _fault_rx) = mpsc::channel(4);

        let handle: ReplicatorHandle = self
            .send_lifecycle(|reply| LifecycleEvent::Open {
                ctx: OpenContext {
                    replica_id: self.replica_id,
                    open_mode: mode,
                    data_bind: self.data_bind.clone(),
                    token: self.shutdown.child_token(),
                    fault_tx,
                },
                reply,
            })
            .await?;

        // 2. Open the replicator (via channel)
        handle
            .send_control(
                |r| ReplicatorControlEvent::Open { mode, reply: r },
                self.reply_timeout,
            )
            .await?;

        info!(
            data_address = %handle.data_address(),
            "replicator opened"
        );

        // 3. Store handle for future lifecycle calls
        self.replicator_handle = Some(handle);
        Ok(())
    }

    async fn handle_change_role(&mut self, epoch: Epoch, new_role: Role) -> Result<()> {
        let old_role = self.role;
        let is_promotion = new_role == Role::Primary
            || (new_role == Role::ActiveSecondary && old_role == Role::IdleSecondary);

        let handle = self.require_handle()?;

        if is_promotion {
            // Promotion: replicator first, then status, then user
            handle
                .send_control(
                    |reply| ReplicatorControlEvent::ChangeRole {
                        epoch,
                        role: new_role,
                        reply,
                    },
                    self.reply_timeout,
                )
                .await?;
            self.set_status_for_role(new_role);
            let _: String = self
                .send_lifecycle(|reply| LifecycleEvent::ChangeRole { new_role, reply })
                .await?;
        } else {
            // Demotion: status first, then user, then replicator
            self.set_status_for_role(new_role);
            let _: String = self
                .send_lifecycle(|reply| LifecycleEvent::ChangeRole { new_role, reply })
                .await?;
            handle
                .send_control(
                    |reply| ReplicatorControlEvent::ChangeRole {
                        epoch,
                        role: new_role,
                        reply,
                    },
                    self.reply_timeout,
                )
                .await?;
        }

        self.role = new_role;
        self.epoch = epoch;
        Ok(())
    }

    async fn handle_close(&mut self) -> Result<()> {
        if let Some(handle) = &self.replicator_handle {
            handle
                .state()
                .set_read_status(AccessStatus::ReconfigurationPending);
            handle
                .state()
                .set_write_status(AccessStatus::ReconfigurationPending);
        }

        let _ = self
            .send_lifecycle(|reply| LifecycleEvent::Close { reply })
            .await;

        if let Ok(handle) = self.require_handle() {
            let _ = handle
                .send_control(
                    |reply| ReplicatorControlEvent::Close { reply },
                    self.reply_timeout,
                )
                .await;
            handle.state().set_read_status(AccessStatus::NotPrimary);
            handle.state().set_write_status(AccessStatus::NotPrimary);
        }

        self.role = Role::None;
        Ok(())
    }

    async fn handle_update_epoch(&mut self, epoch: Epoch) -> Result<()> {
        // Route entirely through replicator — it handles user notification
        self.send_replicator_control(|reply| ReplicatorControlEvent::UpdateEpoch { epoch, reply })
            .await?;
        self.epoch = epoch;
        Ok(())
    }

    async fn handle_on_data_loss(&mut self) -> Result<DataLossAction> {
        // Route through replicator — it handles dual-query (replicator + user)
        self.send_replicator_control(|reply| ReplicatorControlEvent::OnDataLoss { reply })
            .await
    }

    // -----------------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------------

    async fn send_replicator_control<T>(
        &self,
        make: impl FnOnce(oneshot::Sender<Result<T>>) -> ReplicatorControlEvent,
    ) -> Result<T> {
        let handle = self.require_handle()?;
        handle.send_control(make, self.reply_timeout).await
    }

    async fn send_lifecycle<T>(
        &self,
        make: impl FnOnce(oneshot::Sender<Result<T>>) -> LifecycleEvent,
    ) -> Result<T> {
        let (tx, rx) = oneshot::channel();
        self.lifecycle_tx
            .send(make(tx))
            .await
            .map_err(|_| KubelicateError::Closed)?;
        match tokio::time::timeout(self.reply_timeout, rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(KubelicateError::Closed),
            Err(_) => {
                warn!("lifecycle event reply timed out");
                Err(KubelicateError::Internal("lifecycle timeout".into()))
            }
        }
    }

    fn set_status_for_role(&self, role: Role) {
        if let Some(handle) = &self.replicator_handle {
            match role {
                Role::Primary => {
                    handle.state().set_read_status(AccessStatus::Granted);
                    handle.state().set_write_status(AccessStatus::Granted);
                }
                _ => {
                    handle.state().set_read_status(AccessStatus::NotPrimary);
                    handle.state().set_write_status(AccessStatus::NotPrimary);
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers for copy protocol (OperationStream ↔ Vec materialization)
// ---------------------------------------------------------------------------

/// Collect all operations from a stream into a Vec.
async fn collect_stream(mut stream: crate::types::OperationStream) -> Vec<(Lsn, bytes::Bytes)> {
    let mut ops = Vec::new();
    while let Some(op) = stream.get_operation().await {
        ops.push((op.lsn, op.data.clone()));
        op.acknowledge();
    }
    ops
}

/// Create an OperationStream from materialized operations.
fn vec_to_stream(ops: Vec<(Lsn, bytes::Bytes)>) -> crate::types::OperationStream {
    let (tx, stream) = crate::types::OperationStream::channel(ops.len().max(1));
    tokio::spawn(async move {
        for (lsn, data) in ops {
            let op = crate::types::Operation::new(lsn, data, None);
            if tx.send(op).await.is_err() {
                break;
            }
        }
    });
    stream
}

// ---------------------------------------------------------------------------
// Copy protocol (BuildReplica)
// ---------------------------------------------------------------------------

/// Copy phase of BuildReplica — runs as a spawned async task.
/// Uses state_provider_tx from the ReplicatorHandle to reach the user's
/// state provider callbacks.
async fn run_build_replica_copy(
    replica: ReplicaInfo,
    state_provider_tx: mpsc::Sender<crate::events::StateProviderEvent>,
    state: std::sync::Arc<crate::handles::PartitionState>,
    reply_timeout: Duration,
) -> Result<()> {
    use crate::proto::replicator_data_client::ReplicatorDataClient;

    let secondary_addr = &replica.replicator_address;
    info!(
        secondary_id = replica.id,
        %secondary_addr,
        "BuildReplica: connecting to secondary data plane"
    );

    let channel = tonic::transport::Channel::from_shared(secondary_addr.clone())
        .map_err(|e| KubelicateError::Internal(Box::new(e)))?
        .connect()
        .await
        .map_err(|e| KubelicateError::Internal(Box::new(e)))?;
    let mut data_client = ReplicatorDataClient::new(channel);

    let ctx_resp = data_client
        .get_copy_context(crate::proto::GetCopyContextRequest {})
        .await
        .map_err(|e| KubelicateError::Internal(Box::new(e)))?;
    let copy_context_ops = ctx_resp.into_inner().operations;
    info!(
        context_items = copy_context_ops.len(),
        "BuildReplica: got copy context from secondary"
    );

    let copy_context = vec_to_stream(
        copy_context_ops
            .into_iter()
            .map(|op| (op.lsn, bytes::Bytes::from(op.data)))
            .collect(),
    );

    let up_to_lsn = state.committed_lsn();
    let state_stream: crate::types::OperationStream = {
        let (tx, rx) = oneshot::channel();
        state_provider_tx
            .send(crate::events::StateProviderEvent::GetCopyState {
                up_to_lsn,
                copy_context,
                reply: tx,
            })
            .await
            .map_err(|_| KubelicateError::Closed)?;
        match tokio::time::timeout(reply_timeout, rx).await {
            Ok(Ok(result)) => result?,
            Ok(Err(_)) => return Err(KubelicateError::Closed),
            Err(_) => return Err(KubelicateError::Internal("state_provider timeout".into())),
        }
    };

    let state_ops = collect_stream(state_stream).await;

    let copy_lsn = state_ops.iter().map(|(lsn, _)| *lsn).max().unwrap_or(0);
    state.set_copy_lsn(replica.id, copy_lsn);

    info!(
        items = state_ops.len(),
        up_to_lsn, copy_lsn, "BuildReplica: got copy state from local StateProvider"
    );

    let items: Vec<crate::proto::CopyItem> = state_ops
        .into_iter()
        .map(|(lsn, data)| crate::proto::CopyItem {
            lsn,
            data: data.to_vec(),
        })
        .collect();

    let item_stream = tokio_stream::iter(items);
    let resp = data_client
        .copy_stream(item_stream)
        .await
        .map_err(|e| KubelicateError::Internal(Box::new(e)))?;

    info!(
        items_received = resp.into_inner().items_received,
        "BuildReplica: copy complete"
    );

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::LifecycleEvent;
    use crate::replicator::WalReplicator;

    #[tokio::test]
    async fn test_pod_runtime_user_lifecycle() {
        let bundle = PodRuntime::builder(1)
            .reply_timeout(Duration::from_secs(5))
            .build()
            .await
            .unwrap();

        let runtime = bundle.runtime;
        let mut lifecycle_rx = bundle.lifecycle_rx;

        // Spawn user event loop — creates replicator at Open
        let user_handle = tokio::spawn(async move {
            let mut replicator = None;
            let mut replicated_lsns = vec![];

            while let Some(event) = lifecycle_rx.recv().await {
                match event {
                    LifecycleEvent::Open { ctx, reply } => {
                        // User creates replicator and returns handle
                        let (handle, handles) = WalReplicator::create(
                            ctx.replica_id,
                            &ctx.data_bind,
                            ctx.fault_tx.clone(),
                        )
                        .await
                        .unwrap();
                        replicator = Some(handles.replicator);
                        let _ = reply.send(Ok(handle));
                    }
                    LifecycleEvent::ChangeRole { new_role, reply } => {
                        if new_role == Role::Primary {
                            let r = replicator.as_ref().unwrap();
                            let lsn = r
                                .replicate(
                                    bytes::Bytes::from("from-user"),
                                    CancellationToken::new(),
                                )
                                .await
                                .unwrap();
                            replicated_lsns.push(lsn);
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
            replicated_lsns
        });

        // Spawn the runtime command loop
        let runtime_handle = tokio::spawn(runtime.serve());

        // Drive lifecycle via the gRPC control server (simulating operator)
        let mut client = crate::proto::replicator_control_client::ReplicatorControlClient::connect(
            bundle.control_address.clone(),
        )
        .await
        .unwrap();

        // Open
        client
            .open(crate::proto::OpenRequest { mode: 0 })
            .await
            .unwrap();

        // ChangeRole Idle → Active → Primary
        client
            .change_role(crate::proto::ChangeRoleRequest {
                epoch: Some(crate::proto::EpochProto {
                    data_loss_number: 0,
                    configuration_number: 1,
                }),
                role: crate::proto::RoleProto::RoleIdleSecondary as i32,
            })
            .await
            .unwrap();

        client
            .change_role(crate::proto::ChangeRoleRequest {
                epoch: Some(crate::proto::EpochProto {
                    data_loss_number: 0,
                    configuration_number: 1,
                }),
                role: crate::proto::RoleProto::RoleActiveSecondary as i32,
            })
            .await
            .unwrap();

        client
            .change_role(crate::proto::ChangeRoleRequest {
                epoch: Some(crate::proto::EpochProto {
                    data_loss_number: 0,
                    configuration_number: 1,
                }),
                role: crate::proto::RoleProto::RolePrimary as i32,
            })
            .await
            .unwrap();

        // Demote
        client
            .change_role(crate::proto::ChangeRoleRequest {
                epoch: Some(crate::proto::EpochProto {
                    data_loss_number: 0,
                    configuration_number: 2,
                }),
                role: crate::proto::RoleProto::RoleActiveSecondary as i32,
            })
            .await
            .unwrap();

        // Close
        client.close(crate::proto::CloseRequest {}).await.unwrap();

        let lsns = user_handle.await.unwrap();
        assert_eq!(lsns.len(), 1);
        assert_eq!(lsns[0], 1);

        runtime_handle.await.unwrap();
    }
}
