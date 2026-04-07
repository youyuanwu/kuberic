use std::sync::Arc;

use tokio::sync::{mpsc, oneshot};
use tonic::{Request, Response, Status};

use crate::error::KubelicateError;
use crate::events::{ReplicateRequest, ReplicatorControlEvent};
use crate::handles::PartitionState;
use crate::proto::replicator_control_server::ReplicatorControl;
use crate::proto::*;
use crate::types::{Epoch, Role};

/// gRPC control server for a single replica. Routes operator commands
/// to the replicator's control and data channels.
pub struct ControlServer {
    control_tx: mpsc::Sender<ReplicatorControlEvent>,
    data_tx: mpsc::Sender<ReplicateRequest>,
    state: Arc<PartitionState>,
    #[allow(dead_code)]
    replica_id: i64,
    role: std::sync::Mutex<Role>,
    epoch: std::sync::Mutex<Epoch>,
}

impl ControlServer {
    pub fn new(
        replica_id: i64,
        control_tx: mpsc::Sender<ReplicatorControlEvent>,
        data_tx: mpsc::Sender<ReplicateRequest>,
        state: Arc<PartitionState>,
    ) -> Self {
        Self {
            control_tx,
            data_tx,
            state,
            replica_id,
            role: std::sync::Mutex::new(Role::None),
            epoch: std::sync::Mutex::new(Epoch::default()),
        }
    }

    async fn send_control<T>(
        &self,
        make: impl FnOnce(oneshot::Sender<crate::Result<T>>) -> ReplicatorControlEvent,
    ) -> Result<T, Status> {
        let (tx, rx) = oneshot::channel();
        self.control_tx
            .send(make(tx))
            .await
            .map_err(|_| Status::unavailable("replicator closed"))?;
        rx.await
            .map_err(|_| Status::unavailable("replicator closed"))?
            .map_err(|e| Status::internal(e.to_string()))
    }
}

#[tonic::async_trait]
impl ReplicatorControl for ControlServer {
    async fn open(&self, req: Request<OpenRequest>) -> Result<Response<OpenResponse>, Status> {
        let mode = crate::types::OpenMode::from(req.into_inner().mode);
        self.send_control(|reply| ReplicatorControlEvent::Open { mode, reply })
            .await?;
        Ok(Response::new(OpenResponse {}))
    }

    async fn close(&self, _req: Request<CloseRequest>) -> Result<Response<CloseResponse>, Status> {
        self.send_control(|reply| ReplicatorControlEvent::Close { reply })
            .await?;
        Ok(Response::new(CloseResponse {}))
    }

    async fn change_role(
        &self,
        req: Request<ChangeRoleRequest>,
    ) -> Result<Response<ChangeRoleResponse>, Status> {
        let inner = req.into_inner();
        let epoch: Epoch = inner.epoch.unwrap_or_default().into();
        let role: Role = Role::from(inner.role);

        self.send_control(|reply| ReplicatorControlEvent::ChangeRole { epoch, role, reply })
            .await?;

        *self.role.lock().unwrap() = role;
        *self.epoch.lock().unwrap() = epoch;
        Ok(Response::new(ChangeRoleResponse {}))
    }

    async fn update_epoch(
        &self,
        req: Request<UpdateEpochRequest>,
    ) -> Result<Response<UpdateEpochResponse>, Status> {
        let epoch: Epoch = req.into_inner().epoch.unwrap_or_default().into();

        self.send_control(|reply| ReplicatorControlEvent::UpdateEpoch { epoch, reply })
            .await?;

        *self.epoch.lock().unwrap() = epoch;
        Ok(Response::new(UpdateEpochResponse {}))
    }

    async fn get_status(
        &self,
        _req: Request<GetStatusRequest>,
    ) -> Result<Response<GetStatusResponse>, Status> {
        let role = *self.role.lock().unwrap();
        let epoch = *self.epoch.lock().unwrap();

        Ok(Response::new(GetStatusResponse {
            role: RoleProto::from(role) as i32,
            epoch: Some(epoch.into()),
            current_progress: self.state.current_progress(),
            catch_up_capability: self.state.catch_up_capability(),
            committed_lsn: self.state.committed_lsn(),
            healthy: true,
        }))
    }

    async fn update_catch_up_configuration(
        &self,
        req: Request<UpdateCatchUpConfigRequest>,
    ) -> Result<Response<UpdateCatchUpConfigResponse>, Status> {
        let inner = req.into_inner();
        let current = inner.current.unwrap_or_default().into();
        let previous = inner.previous.unwrap_or_default().into();

        let (tx, rx) = oneshot::channel();
        self.control_tx
            .send(ReplicatorControlEvent::UpdateCatchUpConfiguration {
                current,
                previous,
                reply: tx,
            })
            .await
            .map_err(|_| Status::unavailable("replicator closed"))?;
        rx.await
            .map_err(|_| Status::unavailable("replicator closed"))?
            .map_err(|e| Status::internal(e.to_string()))?;

        Ok(Response::new(UpdateCatchUpConfigResponse {}))
    }

    async fn update_current_configuration(
        &self,
        req: Request<UpdateCurrentConfigRequest>,
    ) -> Result<Response<UpdateCurrentConfigResponse>, Status> {
        let current = req.into_inner().current.unwrap_or_default().into();

        let (tx, rx) = oneshot::channel();
        self.control_tx
            .send(ReplicatorControlEvent::UpdateCurrentConfiguration { current, reply: tx })
            .await
            .map_err(|_| Status::unavailable("replicator closed"))?;
        rx.await
            .map_err(|_| Status::unavailable("replicator closed"))?
            .map_err(|e| Status::internal(e.to_string()))?;

        Ok(Response::new(UpdateCurrentConfigResponse {}))
    }

    async fn wait_for_catch_up_quorum(
        &self,
        req: Request<WaitForCatchUpQuorumRequest>,
    ) -> Result<Response<WaitForCatchUpQuorumResponse>, Status> {
        let mode = crate::types::ReplicaSetQuorumMode::from(req.into_inner().mode);
        self.send_control(|reply| ReplicatorControlEvent::WaitForCatchUpQuorum { mode, reply })
            .await?;
        Ok(Response::new(WaitForCatchUpQuorumResponse {}))
    }

    async fn build_replica(
        &self,
        req: Request<BuildReplicaRequest>,
    ) -> Result<Response<BuildReplicaResponse>, Status> {
        let replica = req
            .into_inner()
            .replica
            .ok_or_else(|| Status::invalid_argument("missing replica info"))?
            .into();
        self.send_control(|reply| ReplicatorControlEvent::BuildReplica { replica, reply })
            .await?;
        Ok(Response::new(BuildReplicaResponse {}))
    }

    async fn remove_replica(
        &self,
        req: Request<RemoveReplicaRequest>,
    ) -> Result<Response<RemoveReplicaResponse>, Status> {
        let replica_id = req.into_inner().replica_id;
        self.send_control(|reply| ReplicatorControlEvent::RemoveReplica { replica_id, reply })
            .await?;
        Ok(Response::new(RemoveReplicaResponse {}))
    }

    async fn on_data_loss(
        &self,
        _req: Request<OnDataLossRequest>,
    ) -> Result<Response<OnDataLossResponse>, Status> {
        let action = self
            .send_control(|reply| ReplicatorControlEvent::OnDataLoss { reply })
            .await?;
        Ok(Response::new(OnDataLossResponse {
            state_changed: action == crate::types::DataLossAction::StateChanged,
        }))
    }

    async fn replicate(
        &self,
        req: Request<crate::proto::ReplicateRequest>,
    ) -> Result<Response<ReplicateResponse>, Status> {
        let data = bytes::Bytes::from(req.into_inner().data);
        let (tx, rx) = oneshot::channel();
        self.data_tx
            .send(crate::events::ReplicateRequest { data, reply: tx })
            .await
            .map_err(|_| Status::unavailable("replicator closed"))?;

        let lsn = rx
            .await
            .map_err(|_| Status::unavailable("replicator closed"))?
            .map_err(|e| match e {
                KubelicateError::NotPrimary => Status::failed_precondition("not primary"),
                KubelicateError::NoWriteQuorum => Status::unavailable("no write quorum"),
                KubelicateError::ReconfigurationPending => {
                    Status::unavailable("reconfiguration pending")
                }
                other => Status::internal(other.to_string()),
            })?;

        Ok(Response::new(ReplicateResponse { lsn }))
    }
}

// ---------------------------------------------------------------------------
// ControlServerV2 — routes through RuntimeCommand channel
// ---------------------------------------------------------------------------

use crate::pod::RuntimeCommand;

/// V2 control server that routes all commands through the PodRuntime's
/// command channel. This ensures correct replicator/event ordering.
pub struct ControlServerV2 {
    cmd_tx: mpsc::Sender<RuntimeCommand>,
    #[allow(dead_code)]
    state: Arc<PartitionState>,
    #[allow(dead_code)]
    replica_id: i64,
}

impl ControlServerV2 {
    pub fn new(
        replica_id: i64,
        cmd_tx: mpsc::Sender<RuntimeCommand>,
        state: Arc<PartitionState>,
    ) -> Self {
        Self {
            cmd_tx,
            state,
            replica_id,
        }
    }

    async fn send_cmd<T>(
        &self,
        make: impl FnOnce(oneshot::Sender<crate::Result<T>>) -> RuntimeCommand,
    ) -> Result<T, Status> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(make(tx))
            .await
            .map_err(|_| Status::unavailable("runtime closed"))?;
        rx.await
            .map_err(|_| Status::unavailable("runtime closed"))?
            .map_err(|e| Status::internal(e.to_string()))
    }
}

#[tonic::async_trait]
impl ReplicatorControl for ControlServerV2 {
    async fn open(&self, req: Request<OpenRequest>) -> Result<Response<OpenResponse>, Status> {
        let mode = crate::types::OpenMode::from(req.into_inner().mode);
        self.send_cmd(|reply| RuntimeCommand::Open { mode, reply })
            .await?;
        Ok(Response::new(OpenResponse {}))
    }

    async fn close(&self, _req: Request<CloseRequest>) -> Result<Response<CloseResponse>, Status> {
        self.send_cmd(|reply| RuntimeCommand::Close { reply })
            .await?;
        Ok(Response::new(CloseResponse {}))
    }

    async fn change_role(
        &self,
        req: Request<ChangeRoleRequest>,
    ) -> Result<Response<ChangeRoleResponse>, Status> {
        let inner = req.into_inner();
        let epoch: Epoch = inner.epoch.unwrap_or_default().into();
        let role: Role = Role::from(inner.role);
        self.send_cmd(|reply| RuntimeCommand::ChangeRole { epoch, role, reply })
            .await?;
        Ok(Response::new(ChangeRoleResponse {}))
    }

    async fn update_epoch(
        &self,
        req: Request<UpdateEpochRequest>,
    ) -> Result<Response<UpdateEpochResponse>, Status> {
        let epoch: Epoch = req.into_inner().epoch.unwrap_or_default().into();
        self.send_cmd(|reply| RuntimeCommand::UpdateEpoch { epoch, reply })
            .await?;
        Ok(Response::new(UpdateEpochResponse {}))
    }

    async fn get_status(
        &self,
        _req: Request<GetStatusRequest>,
    ) -> Result<Response<GetStatusResponse>, Status> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(RuntimeCommand::GetStatus { reply: tx })
            .await
            .map_err(|_| Status::unavailable("runtime closed"))?;
        let info = rx
            .await
            .map_err(|_| Status::unavailable("runtime closed"))?;
        Ok(Response::new(GetStatusResponse {
            role: crate::proto::RoleProto::from(info.role) as i32,
            epoch: Some(info.epoch.into()),
            current_progress: info.current_progress,
            catch_up_capability: info.catch_up_capability,
            committed_lsn: info.committed_lsn,
            healthy: info.healthy,
        }))
    }

    async fn update_catch_up_configuration(
        &self,
        req: Request<UpdateCatchUpConfigRequest>,
    ) -> Result<Response<UpdateCatchUpConfigResponse>, Status> {
        let inner = req.into_inner();
        self.send_cmd(|reply| RuntimeCommand::UpdateCatchUpConfiguration {
            current: inner.current.unwrap_or_default().into(),
            previous: inner.previous.unwrap_or_default().into(),
            reply,
        })
        .await?;
        Ok(Response::new(UpdateCatchUpConfigResponse {}))
    }

    async fn update_current_configuration(
        &self,
        req: Request<UpdateCurrentConfigRequest>,
    ) -> Result<Response<UpdateCurrentConfigResponse>, Status> {
        let inner = req.into_inner();
        self.send_cmd(|reply| RuntimeCommand::UpdateCurrentConfiguration {
            current: inner.current.unwrap_or_default().into(),
            reply,
        })
        .await?;
        Ok(Response::new(UpdateCurrentConfigResponse {}))
    }

    async fn wait_for_catch_up_quorum(
        &self,
        req: Request<WaitForCatchUpQuorumRequest>,
    ) -> Result<Response<WaitForCatchUpQuorumResponse>, Status> {
        let mode = crate::types::ReplicaSetQuorumMode::from(req.into_inner().mode);
        self.send_cmd(|reply| RuntimeCommand::WaitForCatchUpQuorum { mode, reply })
            .await?;
        Ok(Response::new(WaitForCatchUpQuorumResponse {}))
    }

    async fn build_replica(
        &self,
        req: Request<BuildReplicaRequest>,
    ) -> Result<Response<BuildReplicaResponse>, Status> {
        let replica = req
            .into_inner()
            .replica
            .ok_or_else(|| Status::invalid_argument("missing replica"))?
            .into();
        self.send_cmd(|reply| RuntimeCommand::BuildReplica { replica, reply })
            .await?;
        Ok(Response::new(BuildReplicaResponse {}))
    }

    async fn remove_replica(
        &self,
        req: Request<RemoveReplicaRequest>,
    ) -> Result<Response<RemoveReplicaResponse>, Status> {
        let replica_id = req.into_inner().replica_id;
        self.send_cmd(|reply| RuntimeCommand::RemoveReplica { replica_id, reply })
            .await?;
        Ok(Response::new(RemoveReplicaResponse {}))
    }

    async fn on_data_loss(
        &self,
        _req: Request<OnDataLossRequest>,
    ) -> Result<Response<OnDataLossResponse>, Status> {
        let action = self
            .send_cmd(|reply| RuntimeCommand::OnDataLoss { reply })
            .await?;
        Ok(Response::new(OnDataLossResponse {
            state_changed: action == crate::types::DataLossAction::StateChanged,
        }))
    }

    async fn replicate(
        &self,
        req: Request<crate::proto::ReplicateRequest>,
    ) -> Result<Response<ReplicateResponse>, Status> {
        let data = bytes::Bytes::from(req.into_inner().data);
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(RuntimeCommand::Replicate { data, reply: tx })
            .await
            .map_err(|_| Status::unavailable("runtime closed"))?;
        let lsn = rx
            .await
            .map_err(|_| Status::unavailable("runtime closed"))?
            .map_err(|e| match e {
                KubelicateError::NotPrimary => Status::failed_precondition("not primary"),
                KubelicateError::NoWriteQuorum => Status::unavailable("no write quorum"),
                other => Status::internal(other.to_string()),
            })?;
        Ok(Response::new(ReplicateResponse { lsn }))
    }
}
