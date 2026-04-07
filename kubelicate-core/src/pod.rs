use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{mpsc, oneshot};
use tonic::transport::Server;
use tracing::info;

use crate::error::{KubelicateError, Result};
use crate::events::{
    ReplicateRequest, ReplicatorChannels, ReplicatorControlEvent, ServiceContext, ServiceEvent,
};
use crate::handles::{PartitionHandle, PartitionState, StateReplicatorHandle};
use crate::proto::replicator_data_server::ReplicatorDataServer;
use crate::replicator::actor::WalReplicatorActor;
use crate::replicator::secondary::{SecondaryReceiver, SecondaryState};
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
    Replicate {
        data: bytes::Bytes,
        reply: oneshot::Sender<Result<Lsn>>,
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
    control_tx: mpsc::Sender<ReplicatorControlEvent>,
    data_tx: mpsc::Sender<ReplicateRequest>,
    service_tx: mpsc::Sender<ServiceEvent>,
    cmd_rx: mpsc::Receiver<RuntimeCommand>,
    state: Arc<PartitionState>,
    shutdown: CancellationToken,
    reply_timeout: Duration,
    role: Role,
    epoch: Epoch,
}

pub struct PodRuntimeBundle {
    pub runtime: PodRuntime,
    pub service_rx: mpsc::Receiver<ServiceEvent>,
    pub control_address: String,
    pub data_address: String,
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
        let channels = ReplicatorChannels::new(16, 256);
        let state = Arc::new(PartitionState::new());
        let secondary_state = Arc::new(SecondaryState::new());
        let shutdown = CancellationToken::new();
        let (service_tx, service_rx) = mpsc::channel(16);
        let (cmd_tx, cmd_rx) = mpsc::channel(16);

        // Start data plane gRPC server
        let data_receiver = SecondaryReceiver::new(secondary_state);
        let data_listener = tokio::net::TcpListener::bind(&self.data_bind)
            .await
            .map_err(|e| KubelicateError::Internal(Box::new(e)))?;
        let data_addr = data_listener.local_addr().unwrap();
        let data_address = format!("http://{}", data_addr);

        let data_shutdown = shutdown.child_token();
        tokio::spawn(async move {
            let _ = Server::builder()
                .add_service(ReplicatorDataServer::new(data_receiver))
                .serve_with_incoming_shutdown(
                    tokio_stream::wrappers::TcpListenerStream::new(data_listener),
                    data_shutdown.cancelled(),
                )
                .await;
        });

        // Start control plane gRPC server (routes through cmd_tx → runtime)
        let control_server =
            crate::grpc::server::ControlServerV2::new(self.replica_id, cmd_tx, state.clone());
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

        // Start replicator actor
        let actor = WalReplicatorActor::new(self.replica_id);
        let state_cp = state.clone();
        tokio::spawn(async move {
            actor
                .run(channels.control_rx, channels.data_rx, state_cp)
                .await;
        });

        tokio::time::sleep(Duration::from_millis(50)).await;

        info!(
            replica_id = self.replica_id,
            %control_address,
            %data_address,
            "pod runtime started"
        );

        let runtime = PodRuntime {
            control_tx: channels.control_tx,
            data_tx: channels.data_tx,
            service_tx,
            cmd_rx,
            state,
            shutdown,
            reply_timeout: self.reply_timeout,
            role: Role::None,
            epoch: Epoch::default(),
        };

        Ok(PodRuntimeBundle {
            runtime,
            service_rx,
            control_address,
            data_address,
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
                    let _ = reply.send(self.handle_change_role(epoch, role).await);
                }
                RuntimeCommand::UpdateEpoch { epoch, reply } => {
                    let _ = reply.send(
                        self.send_replicator_control(|r| ReplicatorControlEvent::UpdateEpoch {
                            epoch,
                            reply: r,
                        })
                        .await,
                    );
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
                    let _ = reply.send(
                        self.send_replicator_control(|r| ReplicatorControlEvent::BuildReplica {
                            replica,
                            reply: r,
                        })
                        .await,
                    );
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
                    let _ = reply.send(
                        self.send_replicator_control(|r| ReplicatorControlEvent::OnDataLoss {
                            reply: r,
                        })
                        .await,
                    );
                }
                RuntimeCommand::Replicate { data, reply } => {
                    let (r_tx, r_rx) = oneshot::channel();
                    let _ = self
                        .data_tx
                        .send(ReplicateRequest { data, reply: r_tx })
                        .await;
                    let _ = reply.send(match r_rx.await {
                        Ok(result) => result,
                        Err(_) => Err(KubelicateError::Closed),
                    });
                }
                RuntimeCommand::GetStatus { reply } => {
                    let _ = reply.send(StatusInfo {
                        role: self.role,
                        epoch: self.epoch,
                        current_progress: self.state.current_progress(),
                        catch_up_capability: self.state.catch_up_capability(),
                        committed_lsn: self.state.committed_lsn(),
                        healthy: true,
                    });
                }
            }
        }
        self.shutdown.cancel();
    }

    // -----------------------------------------------------------------------
    // Command handlers with correct ordering
    // -----------------------------------------------------------------------

    async fn handle_open(&mut self, mode: OpenMode) -> Result<()> {
        // 1. Open replicator
        self.send_replicator_control(|reply| ReplicatorControlEvent::Open { mode, reply })
            .await?;

        // 2. Create handles and deliver Open to user
        let partition = Arc::new(PartitionHandle::new(self.state.clone(), mpsc::channel(4).0));
        let replicator_handle =
            StateReplicatorHandle::new(self.data_tx.clone(), self.state.clone());

        self.send_service(|reply| ServiceEvent::Open {
            ctx: ServiceContext {
                partition,
                replicator: replicator_handle,
                token: self.shutdown.child_token(),
            },
            reply,
        })
        .await?;

        Ok(())
    }

    async fn handle_change_role(&mut self, epoch: Epoch, new_role: Role) -> Result<()> {
        let old_role = self.role;
        let is_promotion = new_role == Role::Primary
            || (new_role == Role::ActiveSecondary && old_role == Role::IdleSecondary);

        if is_promotion {
            // Promotion: replicator first, then status, then user
            self.send_replicator_control(|reply| ReplicatorControlEvent::ChangeRole {
                epoch,
                role: new_role,
                reply,
            })
            .await?;
            self.set_status_for_role(new_role);
            let _: String = self
                .send_service(|reply| ServiceEvent::ChangeRole { new_role, reply })
                .await?;
        } else {
            // Demotion: status first, then user, then replicator
            self.set_status_for_role(new_role);
            let _: String = self
                .send_service(|reply| ServiceEvent::ChangeRole { new_role, reply })
                .await?;
            self.send_replicator_control(|reply| ReplicatorControlEvent::ChangeRole {
                epoch,
                role: new_role,
                reply,
            })
            .await?;
        }

        self.role = new_role;
        self.epoch = epoch;
        Ok(())
    }

    async fn handle_close(&mut self) -> Result<()> {
        self.state
            .set_read_status(AccessStatus::ReconfigurationPending);
        self.state
            .set_write_status(AccessStatus::ReconfigurationPending);

        let _ = self
            .send_service(|reply| ServiceEvent::Close { reply })
            .await;
        let _ = self
            .send_replicator_control(|reply| ReplicatorControlEvent::Close { reply })
            .await;

        self.state.set_read_status(AccessStatus::NotPrimary);
        self.state.set_write_status(AccessStatus::NotPrimary);
        self.role = Role::None;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------------

    async fn send_replicator_control<T>(
        &self,
        make: impl FnOnce(oneshot::Sender<Result<T>>) -> ReplicatorControlEvent,
    ) -> Result<T> {
        let (tx, rx) = oneshot::channel();
        self.control_tx
            .send(make(tx))
            .await
            .map_err(|_| KubelicateError::Closed)?;
        match tokio::time::timeout(self.reply_timeout, rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(KubelicateError::Closed),
            Err(_) => Err(KubelicateError::Internal("replicator timeout".into())),
        }
    }

    async fn send_service<T>(
        &self,
        make: impl FnOnce(oneshot::Sender<Result<T>>) -> ServiceEvent,
    ) -> Result<T> {
        let (tx, rx) = oneshot::channel();
        self.service_tx
            .send(make(tx))
            .await
            .map_err(|_| KubelicateError::Closed)?;
        match tokio::time::timeout(self.reply_timeout, rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(KubelicateError::Closed),
            Err(_) => Err(KubelicateError::Internal("service timeout".into())),
        }
    }

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

    #[tokio::test]
    async fn test_pod_runtime_user_lifecycle() {
        let bundle = PodRuntime::builder(1)
            .reply_timeout(Duration::from_secs(5))
            .build()
            .await
            .unwrap();

        let runtime = bundle.runtime;
        let mut service_rx = bundle.service_rx;

        // Spawn user event loop
        let user_handle = tokio::spawn(async move {
            let mut _partition = None;
            let mut replicator = None;
            let mut replicated_lsns = vec![];

            while let Some(event) = service_rx.recv().await {
                match event {
                    ServiceEvent::Open { ctx, reply } => {
                        _partition = Some(ctx.partition.clone());
                        replicator = Some(ctx.replicator);
                        let _ = reply.send(Ok(()));
                    }
                    ServiceEvent::ChangeRole { new_role, reply } => {
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
                    ServiceEvent::Close { reply } => {
                        let _ = reply.send(Ok(()));
                        break;
                    }
                    ServiceEvent::Abort => break,
                }
            }
            replicated_lsns
        });

        // Spawn the runtime command loop
        let runtime_handle = tokio::spawn(runtime.serve());

        // Drive lifecycle via the gRPC control server (simulating operator)
        // Connect as a gRPC client to the control address
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
