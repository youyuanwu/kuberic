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

        let signal_path = self.instance.data_dir().join("standby.signal");
        let was_primary = !signal_path.exists();

        if was_primary {
            // This instance was a primary (no standby.signal) and has potentially
            // diverged WAL past the new primary's fork point. Try pg_rewind first
            // (fast — copies only diverged pages), fall back to full pg_basebackup.
            tracing::info!("ReconfigureStandby: demoted primary — attempting pg_rewind");

            if let Err(e) = self.instance.stop().await {
                tracing::warn!("stop before rewind: {}", e);
            }

            // Try pg_rewind first (requires data checksums + wal_log_hints = on)
            let rewind_result = self
                .instance
                .rewind(&req.primary_host, req.primary_port as u16)
                .await;

            if let Err(e) = &rewind_result {
                tracing::warn!("pg_rewind failed, falling back to pg_basebackup: {}", e);

                let data_dir = self.instance.data_dir().to_path_buf();
                if let Err(e) = clear_data_dir(&data_dir).await {
                    return Ok(Response::new(crate::proto::ReconfigureStandbyResponse {
                        success: false,
                        error: format!("clear data_dir for rebuild: {e}"),
                    }));
                }

                if let Err(e) = self
                    .instance
                    .base_backup(&req.primary_host, req.primary_port as u16)
                    .await
                {
                    let _ = clear_data_dir(&data_dir).await;
                    return Ok(Response::new(crate::proto::ReconfigureStandbyResponse {
                        success: false,
                        error: format!("pg_basebackup fallback: {e}"),
                    }));
                }
            }

            // After rewind or basebackup: create standby.signal + patch config
            let signal_path = self.instance.data_dir().join("standby.signal");
            if !signal_path.exists() && tokio::fs::File::create(&signal_path).await.is_err() {
                return Ok(Response::new(crate::proto::ReconfigureStandbyResponse {
                    success: false,
                    error: "create standby.signal".to_string(),
                }));
            }

            // Patch config (fix port, socket_dir, application_name)
            if let Err(e) = self
                .instance
                .config()
                .patch_after_clone(self.instance.data_dir(), req.replica_id)
                .await
            {
                return Ok(Response::new(crate::proto::ReconfigureStandbyResponse {
                    success: false,
                    error: format!("patch config after rejoin: {e}"),
                }));
            }

            if let Err(e) = self.instance.start(self.fault_tx.clone()).await {
                return Ok(Response::new(crate::proto::ReconfigureStandbyResponse {
                    success: false,
                    error: format!("start after rejoin: {e}"),
                }));
            }

            let method = if rewind_result.is_ok() {
                "pg_rewind"
            } else {
                "pg_basebackup"
            };
            tracing::info!(method, "ReconfigureStandby: rejoined as standby");
        } else {
            // Already a standby — just update primary_conninfo and restart.
            // Timeline following should work since we haven't diverged.
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
        }

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
