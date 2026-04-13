use std::sync::atomic::{AtomicI64, Ordering};

use async_trait::async_trait;
use tonic::transport::Channel;

use crate::driver::ReplicaHandle;
use crate::error::{KubericError, Result};
use crate::proto::replicator_control_client::ReplicatorControlClient;
use crate::proto::*;
use crate::types::{
    DataLossAction, Epoch, Lsn, OpenMode, ReplicaId, ReplicaInfo, ReplicaSetConfig,
    ReplicaSetQuorumMode, ReplicaStatusInfo, Role,
};

/// Implements `ReplicaHandle` by calling a remote pod's gRPC `ReplicatorControl` service.
/// Used by the operator to drive remote replicas.
pub struct GrpcReplicaHandle {
    id: ReplicaId,
    client: ReplicatorControlClient<Channel>,
    /// The data plane address (secondary gRPC server for replication streams).
    data_address: String,
    current_progress: AtomicI64,
    catch_up_capability: AtomicI64,
}

impl GrpcReplicaHandle {
    pub async fn connect(
        id: ReplicaId,
        control_address: String,
        data_address: String,
    ) -> Result<Self> {
        let channel = Channel::from_shared(control_address)
            .map_err(|e| KubericError::Internal(Box::new(e)))?
            .connect_timeout(std::time::Duration::from_secs(5))
            .connect()
            .await
            .map_err(|e| KubericError::Internal(Box::new(e)))?;

        Ok(Self {
            id,
            client: ReplicatorControlClient::new(channel),
            data_address,
            current_progress: AtomicI64::new(0),
            catch_up_capability: AtomicI64::new(0),
        })
    }

    fn map_err(e: tonic::Status) -> KubericError {
        match e.code() {
            tonic::Code::FailedPrecondition => KubericError::NotPrimary,
            tonic::Code::Unavailable => {
                if e.message().contains("no write quorum") {
                    KubericError::NoWriteQuorum
                } else if e.message().contains("reconfiguration") {
                    KubericError::ReconfigurationPending
                } else {
                    KubericError::Internal(Box::new(e))
                }
            }
            _ => KubericError::Internal(Box::new(e)),
        }
    }
}

#[async_trait]
impl ReplicaHandle for GrpcReplicaHandle {
    fn id(&self) -> ReplicaId {
        self.id
    }

    async fn open(&self, mode: OpenMode) -> Result<()> {
        let mut client = self.client.clone();
        client
            .open(OpenRequest {
                mode: OpenModeProto::from(mode) as i32,
            })
            .await
            .map_err(Self::map_err)?;
        Ok(())
    }

    async fn close(&self) -> Result<()> {
        let mut client = self.client.clone();
        client.close(CloseRequest {}).await.map_err(Self::map_err)?;
        Ok(())
    }

    fn abort(&self) {
        // gRPC doesn't have fire-and-forget; best effort
        let mut client = self.client.clone();
        tokio::spawn(async move {
            let _ = client.close(CloseRequest {}).await;
        });
    }

    async fn change_role(&self, epoch: Epoch, role: Role) -> Result<()> {
        let mut client = self.client.clone();
        client
            .change_role(ChangeRoleRequest {
                epoch: Some(epoch.into()),
                role: RoleProto::from(role) as i32,
            })
            .await
            .map_err(Self::map_err)?;
        Ok(())
    }

    async fn update_epoch(&self, epoch: Epoch) -> Result<()> {
        let mut client = self.client.clone();
        client
            .update_epoch(UpdateEpochRequest {
                epoch: Some(epoch.into()),
            })
            .await
            .map_err(Self::map_err)?;
        Ok(())
    }

    fn current_progress(&self) -> Lsn {
        self.current_progress.load(Ordering::Acquire)
    }

    fn catch_up_capability(&self) -> Lsn {
        self.catch_up_capability.load(Ordering::Acquire)
    }

    async fn on_data_loss(&self) -> Result<DataLossAction> {
        let mut client = self.client.clone();
        let resp = client
            .on_data_loss(OnDataLossRequest {})
            .await
            .map_err(Self::map_err)?;
        if resp.into_inner().state_changed {
            Ok(DataLossAction::StateChanged)
        } else {
            Ok(DataLossAction::None)
        }
    }

    async fn update_catch_up_configuration(
        &self,
        current: ReplicaSetConfig,
        previous: ReplicaSetConfig,
    ) -> Result<()> {
        let mut client = self.client.clone();
        client
            .update_catch_up_configuration(UpdateCatchUpConfigRequest {
                current: Some(current.into()),
                previous: Some(previous.into()),
            })
            .await
            .map_err(Self::map_err)?;
        Ok(())
    }

    async fn update_current_configuration(&self, current: ReplicaSetConfig) -> Result<()> {
        let mut client = self.client.clone();
        client
            .update_current_configuration(UpdateCurrentConfigRequest {
                current: Some(current.into()),
            })
            .await
            .map_err(Self::map_err)?;
        Ok(())
    }

    async fn wait_for_catch_up_quorum(&self, mode: ReplicaSetQuorumMode) -> Result<()> {
        let mut client = self.client.clone();
        client
            .wait_for_catch_up_quorum(WaitForCatchUpQuorumRequest {
                mode: QuorumModeProto::from(mode) as i32,
            })
            .await
            .map_err(Self::map_err)?;
        Ok(())
    }

    async fn build_replica(&self, replica: ReplicaInfo) -> Result<()> {
        let mut client = self.client.clone();
        client
            .build_replica(BuildReplicaRequest {
                replica: Some(replica.into()),
            })
            .await
            .map_err(Self::map_err)?;
        Ok(())
    }

    async fn remove_replica(&self, replica_id: ReplicaId) -> Result<()> {
        let mut client = self.client.clone();
        client
            .remove_replica(RemoveReplicaRequest { replica_id })
            .await
            .map_err(Self::map_err)?;
        Ok(())
    }

    async fn revoke_write_status(&self) -> Result<()> {
        let mut client = self.client.clone();
        client
            .revoke_write_status(RevokeWriteStatusRequest {})
            .await
            .map_err(Self::map_err)?;
        Ok(())
    }

    fn replicator_address(&self) -> String {
        self.data_address.clone()
    }

    async fn get_status(&self) -> Result<ReplicaStatusInfo> {
        let mut client = self.client.clone();
        let resp = client
            .get_status(GetStatusRequest {})
            .await
            .map_err(Self::map_err)?;
        let inner = resp.into_inner();
        let epoch = inner.epoch.map(Epoch::from).unwrap_or(Epoch::new(0, 0));
        let role = Role::from(inner.role);

        // Update cached progress as side effect
        self.current_progress
            .store(inner.current_progress, Ordering::Release);
        self.catch_up_capability
            .store(inner.catch_up_capability, Ordering::Release);

        Ok(ReplicaStatusInfo {
            role,
            epoch,
            current_progress: inner.current_progress,
            healthy: inner.healthy,
        })
    }
}
