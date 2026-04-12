use std::sync::Arc;

use kuberic_core::events::LifecycleEvent;
use kuberic_core::handles::PartitionState;
use kuberic_core::types::Role;
use tokio::sync::mpsc;

use crate::adapter::create_pg_replicator;
use crate::data_service::start_data_server;
use crate::instance::PgInstanceManager;
use crate::monitor::PgMonitor;

/// Main PG service event loop. Processes lifecycle events.
///
/// Unlike kvstore/sqlite, this service does NOT create a WalReplicator
/// or handle StateProviderEvents. PostgreSQL manages its own replication.
/// The PgReplicatorAdapter translates control events to PG operations.
pub async fn run_service(
    mut lifecycle_rx: mpsc::Receiver<LifecycleEvent>,
    instance: Arc<PgInstanceManager>,
) {
    let mut _monitor: Option<Arc<PgMonitor>> = None;
    let mut _monitor_handle: Option<tokio::task::JoinHandle<()>> = None;
    let mut _data_server_handle: Option<tokio::task::JoinHandle<()>> = None;

    tracing::info!("pg service started, waiting for events");

    while let Some(event) = lifecycle_rx.recv().await {
        match event {
            LifecycleEvent::Open { ctx, reply } => {
                tracing::info!(replica_id = ctx.replica_id, "Open: creating PG replicator");

                let state = Arc::new(PartitionState::new());
                let mon = Arc::new(PgMonitor::new(instance.clone(), state.clone()));

                // Spawn monitor
                let mon_clone = mon.clone();
                let mon_token = ctx.token.child_token();
                _monitor_handle = Some(tokio::spawn(async move {
                    mon_clone.run(mon_token).await;
                }));

                // Start PgDataService on data_bind (app-to-app coordination)
                let data_address =
                    match start_data_server(&ctx.data_bind, instance.clone(), ctx.fault_tx.clone())
                        .await
                    {
                        Ok((addr, handle)) => {
                            _data_server_handle = Some(handle);
                            addr
                        }
                        Err(e) => {
                            tracing::error!("failed to start data server: {}", e);
                            let _ = reply.send(Err(kuberic_core::KubericError::Internal(
                                format!("data server: {e}").into(),
                            )));
                            continue;
                        }
                    };

                // Create adapter → ReplicatorHandle
                let handle =
                    create_pg_replicator(instance.clone(), state, mon.clone(), data_address);

                _monitor = Some(mon);
                let _ = reply.send(Ok(handle));
            }

            LifecycleEvent::ChangeRole { new_role, reply } => {
                tracing::info!(?new_role, "ChangeRole");

                match new_role {
                    Role::Primary => {
                        let addr = format!("localhost:{}", instance.port());
                        let _ = reply.send(Ok(addr));
                    }
                    _ => {
                        let _ = reply.send(Ok(String::new()));
                    }
                }
            }

            LifecycleEvent::Close { reply } => {
                tracing::info!("Close: PG service shutting down");
                // PG stop is handled by adapter's Close handler
                let _ = reply.send(Ok(()));
                break;
            }

            LifecycleEvent::Abort => {
                tracing::info!("Abort: PG service emergency shutdown");
                // PG stop is handled by adapter's Abort handler
                break;
            }
        }
    }
}
