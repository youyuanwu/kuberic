use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{mpsc, oneshot};
use tonic::transport::Server;
use tracing::{info, warn};

use crate::error::{KubelicateError, Result};
use crate::events::{
    ReplicateRequest, ReplicatorChannels, ReplicatorControlEvent, ServiceContext, ServiceEvent,
};
use crate::grpc::server::ControlServer;
use crate::handles::{PartitionHandle, PartitionState, StateReplicatorHandle};
use crate::proto::replicator_control_server::ReplicatorControlServer;
use crate::proto::replicator_data_server::ReplicatorDataServer;
use crate::replicator::actor::WalReplicatorActor;
use crate::replicator::secondary::{SecondaryReceiver, SecondaryState};
use crate::types::{AccessStatus, CancellationToken, Epoch, FaultType, OpenMode, ReplicaId, Role};

const DEFAULT_REPLY_TIMEOUT: Duration = Duration::from_secs(30);

/// A fully wired pod runtime that manages:
/// - WalReplicatorActor (replication engine)
/// - gRPC control server (operator → pod)
/// - gRPC data server (pod ↔ pod replication)
/// - ServiceEvent delivery to user code
/// - Access status transitions
///
/// This is what a real application pod runs.
pub struct PodRuntime {
    replica_id: ReplicaId,
    control_tx: mpsc::Sender<ReplicatorControlEvent>,
    data_tx: mpsc::Sender<ReplicateRequest>,
    service_tx: mpsc::Sender<ServiceEvent>,
    state: Arc<PartitionState>,
    shutdown: CancellationToken,
    reply_timeout: Duration,
    role: Role,
    epoch: Epoch,
}

/// Everything produced by the PodRuntime builder.
pub struct PodRuntimeBundle {
    pub runtime: PodRuntime,
    /// User receives lifecycle events here.
    pub service_rx: mpsc::Receiver<ServiceEvent>,
    /// The control server address (for operator to connect to).
    pub control_address: String,
    /// The data server address (for other replicas to connect to).
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

    /// Build and start the pod runtime. Spawns actor + gRPC servers.
    pub async fn build(self) -> Result<PodRuntimeBundle> {
        let channels = ReplicatorChannels::new(16, 256);
        let state = Arc::new(PartitionState::new());
        let secondary_state = Arc::new(SecondaryState::new());
        let shutdown = CancellationToken::new();
        let (service_tx, service_rx) = mpsc::channel(16);
        let (fault_tx, _fault_rx) = mpsc::channel::<FaultType>(4);

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

        // Start control plane gRPC server
        let control_server = ControlServer::new(
            self.replica_id,
            channels.control_tx.clone(),
            channels.data_tx.clone(),
            state.clone(),
        );
        let control_listener = tokio::net::TcpListener::bind(&self.control_bind)
            .await
            .map_err(|e| KubelicateError::Internal(Box::new(e)))?;
        let control_addr = control_listener.local_addr().unwrap();
        let control_address = format!("http://{}", control_addr);

        let ctrl_shutdown = shutdown.child_token();
        tokio::spawn(async move {
            let _ = Server::builder()
                .add_service(ReplicatorControlServer::new(control_server))
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

        // Give servers time to start
        tokio::time::sleep(Duration::from_millis(50)).await;

        let _partition = Arc::new(PartitionHandle::new(state.clone(), fault_tx));

        info!(
            replica_id = self.replica_id,
            %control_address,
            %data_address,
            "pod runtime started"
        );

        let runtime = PodRuntime {
            replica_id: self.replica_id,
            control_tx: channels.control_tx,
            data_tx: channels.data_tx,
            service_tx,
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

    pub fn replica_id(&self) -> ReplicaId {
        self.replica_id
    }

    pub fn shutdown(&self) {
        self.shutdown.cancel();
    }

    // -----------------------------------------------------------------------
    // Lifecycle operations (called by operator via control server, or
    // directly in tests)
    // -----------------------------------------------------------------------

    /// Open the replicator, then deliver ServiceEvent::Open to user.
    pub async fn open(&mut self, mode: OpenMode) -> Result<()> {
        // 1. Open replicator
        self.send_control(|reply| ReplicatorControlEvent::Open { mode, reply })
            .await?;

        // 2. Create user-facing handles
        let partition = Arc::new(PartitionHandle::new(
            self.state.clone(),
            mpsc::channel(4).0, // fault channel
        ));
        let replicator_handle =
            StateReplicatorHandle::new(self.data_tx.clone(), self.state.clone());

        // 3. Deliver Open to user
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

    /// Change role with correct promotion/demotion ordering.
    pub async fn change_role(&mut self, epoch: Epoch, new_role: Role) -> Result<()> {
        let old_role = self.role;
        let is_promotion = new_role == Role::Primary
            || (new_role == Role::ActiveSecondary && old_role == Role::IdleSecondary);

        if is_promotion {
            // Promotion: replicator first, then user
            self.send_control(|reply| ReplicatorControlEvent::ChangeRole {
                epoch,
                role: new_role,
                reply,
            })
            .await?;

            self.set_status_for_role(new_role);

            let _addr: String = self
                .send_service(|reply| ServiceEvent::ChangeRole { new_role, reply })
                .await?;
        } else {
            // Demotion: user first, then replicator
            self.set_status_for_role(new_role);

            let _addr: String = self
                .send_service(|reply| ServiceEvent::ChangeRole { new_role, reply })
                .await?;

            self.send_control(|reply| ReplicatorControlEvent::ChangeRole {
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

    /// Close: user first, then replicator.
    pub async fn close(&mut self) -> Result<()> {
        self.state
            .set_read_status(AccessStatus::ReconfigurationPending);
        self.state
            .set_write_status(AccessStatus::ReconfigurationPending);

        let _ = self
            .send_service(|reply| ServiceEvent::Close { reply })
            .await;

        let _ = self
            .send_control(|reply| ReplicatorControlEvent::Close { reply })
            .await;

        self.state.set_read_status(AccessStatus::NotPrimary);
        self.state.set_write_status(AccessStatus::NotPrimary);
        self.role = Role::None;
        self.shutdown.cancel();
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Internal helpers
    // -----------------------------------------------------------------------

    async fn send_control<T>(
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
            Err(_) => {
                warn!("replicator control event timed out");
                Err(KubelicateError::Internal("reply timeout".into()))
            }
        }
    }

    async fn send_service<T>(
        &self,
        make: impl FnOnce(oneshot::Sender<Result<T>>) -> ServiceEvent,
    ) -> Result<T> {
        let (tx, rx) = oneshot::channel();
        let event = make(tx);

        match tokio::time::timeout(self.reply_timeout, self.service_tx.send(event)).await {
            Ok(Ok(())) => {}
            Ok(Err(_)) => return Err(KubelicateError::Closed),
            Err(_) => {
                warn!("service event send timed out");
                return Err(KubelicateError::Internal("send timeout".into()));
            }
        }

        match tokio::time::timeout(self.reply_timeout, rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(KubelicateError::Closed),
            Err(_) => {
                warn!("service event reply timed out");
                Err(KubelicateError::Internal("reply timeout".into()))
            }
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

        let mut runtime = bundle.runtime;
        let mut service_rx = bundle.service_rx;

        // Spawn user event loop
        let user_handle = tokio::spawn(async move {
            let mut partition = None;
            let mut replicator = None;
            let mut replicated_lsns = vec![];

            while let Some(event) = service_rx.recv().await {
                match event {
                    ServiceEvent::Open { ctx, reply } => {
                        partition = Some(ctx.partition.clone());
                        replicator = Some(ctx.replicator);
                        let _ = reply.send(Ok(()));
                    }
                    ServiceEvent::ChangeRole { new_role, reply } => {
                        if new_role == Role::Primary {
                            // Replicate from user code
                            let r = replicator.as_ref().unwrap();
                            let p = partition.as_ref().unwrap();
                            assert_eq!(p.write_status(), AccessStatus::Granted);

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

        // Drive lifecycle
        runtime.open(OpenMode::New).await.unwrap();

        runtime
            .change_role(Epoch::new(0, 1), Role::IdleSecondary)
            .await
            .unwrap();

        runtime
            .change_role(Epoch::new(0, 1), Role::ActiveSecondary)
            .await
            .unwrap();

        runtime
            .change_role(Epoch::new(0, 1), Role::Primary)
            .await
            .unwrap();

        // Demote
        runtime
            .change_role(Epoch::new(0, 2), Role::ActiveSecondary)
            .await
            .unwrap();

        runtime.close().await.unwrap();

        let lsns = user_handle.await.unwrap();
        assert_eq!(lsns.len(), 1);
        assert_eq!(lsns[0], 1);
    }
}
