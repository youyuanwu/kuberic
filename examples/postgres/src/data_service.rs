use std::sync::Arc;

use kuberic_core::types::FaultType;
use tokio::sync::mpsc;
use tonic::{Request, Response, Status};

use crate::instance::PgInstanceManager;
use crate::proto::pg_data_service_server::{PgDataService, PgDataServiceServer};
use crate::proto::{CloneFromRequest, CloneFromResponse};

/// gRPC service running on each pod's data_bind address.
/// Handles BuildReplica coordination: the primary calls CloneFrom
/// on the secondary to trigger pg_basebackup.
pub struct PgDataServiceImpl {
    instance: Arc<PgInstanceManager>,
    fault_tx: mpsc::Sender<FaultType>,
}

impl PgDataServiceImpl {
    pub fn new(instance: Arc<PgInstanceManager>, fault_tx: mpsc::Sender<FaultType>) -> Self {
        Self { instance, fault_tx }
    }

    pub fn into_server(self) -> PgDataServiceServer<Self> {
        PgDataServiceServer::new(self)
    }
}

#[tonic::async_trait]
impl PgDataService for PgDataServiceImpl {
    async fn clone_from(
        &self,
        request: Request<CloneFromRequest>,
    ) -> Result<Response<CloneFromResponse>, Status> {
        let req = request.into_inner();
        tracing::info!(
            primary_host = %req.primary_host,
            primary_port = req.primary_port,
            "CloneFrom: starting pg_basebackup"
        );

        // 1. Stop PG if running
        if let Err(e) = self.instance.stop().await {
            tracing::warn!("stop before clone: {}", e);
        }

        // 2. Remove existing data_dir contents (keep the directory itself)
        let data_dir = self.instance.data_dir().to_path_buf();
        if let Err(e) = clear_data_dir(&data_dir).await {
            return Ok(Response::new(CloneFromResponse {
                success: false,
                error: format!("failed to clear data_dir: {e}"),
            }));
        }

        // 3. Run pg_basebackup -R (creates standby.signal + primary_conninfo)
        if let Err(e) = self
            .instance
            .base_backup(&req.primary_host, req.primary_port as u16)
            .await
        {
            // Clean up partial data on failure
            let _ = clear_data_dir(&data_dir).await;
            return Ok(Response::new(CloneFromResponse {
                success: false,
                error: format!("pg_basebackup failed: {e}"),
            }));
        }

        // 4. Patch config: fix port/socket_dir + set application_name
        if let Err(e) = self
            .instance
            .config()
            .patch_after_clone(self.instance.data_dir(), req.replica_id)
            .await
        {
            return Ok(Response::new(CloneFromResponse {
                success: false,
                error: format!("patch config failed: {e}"),
            }));
        }

        // 5. Start PG as standby (auto-streams from primary via primary_conninfo)
        if let Err(e) = self.instance.start(self.fault_tx.clone()).await {
            return Ok(Response::new(CloneFromResponse {
                success: false,
                error: format!("start after clone failed: {e}"),
            }));
        }

        tracing::info!("CloneFrom: pg_basebackup complete, streaming from primary");
        Ok(Response::new(CloneFromResponse {
            success: true,
            error: String::new(),
        }))
    }

    async fn reconfigure_standby(
        &self,
        request: Request<crate::proto::ReconfigureStandbyRequest>,
    ) -> Result<Response<crate::proto::ReconfigureStandbyResponse>, Status> {
        let req = request.into_inner();
        tracing::info!(
            primary_host = %req.primary_host,
            primary_port = req.primary_port,
            "ReconfigureStandby: updating primary_conninfo"
        );

        // Update primary_conninfo in postgresql.auto.conf
        if let Err(e) = crate::config::PgConfig::rewrite_primary_conninfo(
            self.instance.data_dir(),
            &req.primary_host,
            req.primary_port,
        )
        .await
        {
            return Ok(Response::new(crate::proto::ReconfigureStandbyResponse {
                success: false,
                error: format!("rewrite primary_conninfo: {e}"),
            }));
        }

        // Restart PG to pick up the new primary_conninfo
        // (pg_reload_conf doesn't reload primary_conninfo — requires restart)
        if let Err(e) = self.instance.stop().await {
            tracing::warn!("stop for reconfigure: {}", e);
        }
        if let Err(e) = self.instance.start(self.fault_tx.clone()).await {
            return Ok(Response::new(crate::proto::ReconfigureStandbyResponse {
                success: false,
                error: format!("restart after reconfigure: {e}"),
            }));
        }

        tracing::info!("ReconfigureStandby: PG restarted with new primary");
        Ok(Response::new(crate::proto::ReconfigureStandbyResponse {
            success: true,
            error: String::new(),
        }))
    }
}

/// Remove all contents of data_dir but keep the directory itself.
async fn clear_data_dir(data_dir: &std::path::Path) -> Result<(), std::io::Error> {
    let mut entries = tokio::fs::read_dir(data_dir).await?;
    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();
        if path.is_dir() {
            tokio::fs::remove_dir_all(&path).await?;
        } else {
            tokio::fs::remove_file(&path).await?;
        }
    }
    Ok(())
}

/// Start the PgDataService gRPC server on the given address.
pub async fn start_data_server(
    bind: &str,
    instance: Arc<PgInstanceManager>,
    fault_tx: mpsc::Sender<FaultType>,
) -> Result<(String, tokio::task::JoinHandle<()>), crate::instance::PgError> {
    let service = PgDataServiceImpl::new(instance, fault_tx);
    let listener = tokio::net::TcpListener::bind(bind)
        .await
        .map_err(|e| crate::instance::PgError::Process(format!("bind data server: {e}")))?;
    let addr = listener.local_addr().unwrap();
    let data_address = format!("http://{addr}");

    let handle = tokio::spawn(async move {
        let _ = tonic::transport::Server::builder()
            .add_service(service.into_server())
            .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(listener))
            .await;
    });

    tracing::info!(%data_address, "PgDataService started");
    Ok((data_address, handle))
}
