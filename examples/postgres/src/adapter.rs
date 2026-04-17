use std::sync::Arc;

use tokio::sync::mpsc;

use kuberic_core::error::Result;
use kuberic_core::events::ReplicatorControlEvent;
use kuberic_core::handles::PartitionState;
use kuberic_core::replicator::ReplicatorHandle;
use kuberic_core::types::{CancellationToken, DataLossAction, Role};

use crate::instance::PgInstanceManager;
use crate::monitor::PgMonitor;

/// Creates a PgReplicatorAdapter that handles all ReplicatorControlEvent
/// variants with PG-specific logic. Returns a ReplicatorHandle for the
/// PodRuntime.
///
/// Unlike WalReplicator, this adapter does NOT ship data — PostgreSQL
/// handles its own streaming replication. The adapter translates kuberic
/// control events into PG operations (promote, stop, configure standbys, etc.).
pub fn create_pg_replicator(
    instance: Arc<PgInstanceManager>,
    state: Arc<PartitionState>,
    monitor: Arc<PgMonitor>,
    data_address: String,
) -> ReplicatorHandle {
    let (control_tx, mut control_rx) = mpsc::channel::<ReplicatorControlEvent>(16);
    let shutdown = CancellationToken::new();

    let shutdown_token = shutdown.clone();
    let self_instance = instance.clone();
    tokio::spawn(async move {
        let mut current_role = Role::None;

        while let Some(event) = control_rx.recv().await {
            match event {
                ReplicatorControlEvent::Open { reply, .. } => {
                    // PG already started in LifecycleEvent::Open
                    let _ = reply.send(Ok(()));
                }

                ReplicatorControlEvent::ChangeRole { role, reply, .. } => {
                    if role == current_role {
                        let _ = reply.send(Ok(()));
                        continue;
                    }

                    let result = handle_change_role(&instance, &monitor, role).await;
                    if result.is_ok() {
                        current_role = role;
                    }
                    let _ = reply.send(result);
                }

                ReplicatorControlEvent::UpdateEpoch { reply, .. } => {
                    // Epoch change on secondaries. The secondary doesn't need
                    // to check for WAL divergence here — if it was a zombie
                    // primary, the operator already removed it from the replica
                    // set. Surviving secondaries get ReconfigureStandby via
                    // UpdateCatchUpConfiguration to reconnect to the new primary.
                    // PG handles timeline following automatically.
                    tracing::info!("UpdateEpoch received");
                    let _ = reply.send(Ok(()));
                }

                ReplicatorControlEvent::BuildReplica { replica, reply } => {
                    tracing::info!(
                        replica_id = replica.id,
                        addr = %replica.replicator_address,
                        "BuildReplica: calling CloneFrom on secondary"
                    );
                    // Connect to secondary's PgDataService and tell it
                    // to pg_basebackup from us (the primary).
                    // Primary PG listens on 0.0.0.0:port — secondary needs
                    // a routable address. For local tests, use 127.0.0.1.
                    // For K8s, this would be the pod IP.
                    let primary_host = instance.listen_host().to_string();
                    let primary_port = instance.port();
                    let secondary_addr = replica.replicator_address.clone();
                    let result = async {
                        use crate::proto::pg_data_service_client::PgDataServiceClient;
                        let mut client = PgDataServiceClient::connect(secondary_addr)
                            .await
                            .map_err(|e| {
                                kuberic_core::KubericError::Internal(
                                    format!("connect to secondary data service: {e}").into(),
                                )
                            })?;
                        let resp = client
                            .clone_from(crate::proto::CloneFromRequest {
                                primary_host,
                                primary_port: primary_port as u32,
                                replica_id: replica.id,
                            })
                            .await
                            .map_err(|e| {
                                kuberic_core::KubericError::Internal(
                                    format!("CloneFrom RPC: {e}").into(),
                                )
                            })?;
                        let resp = resp.into_inner();
                        if resp.success {
                            Ok(())
                        } else {
                            Err(kuberic_core::KubericError::Internal(
                                format!("CloneFrom failed: {}", resp.error).into(),
                            ))
                        }
                    }
                    .await;
                    let _ = reply.send(result);
                }

                ReplicatorControlEvent::UpdateCatchUpConfiguration { current, reply, .. } => {
                    // 1. Reconfigure secondaries to point to this primary
                    let primary_host = self_instance.listen_host().to_string();
                    let primary_port = self_instance.port() as u32;
                    for member in &current.members {
                        let addr = member.replicator_address.clone();
                        if let Err(e) =
                            reconfigure_secondary(&addr, &primary_host, primary_port, member.id)
                                .await
                        {
                            tracing::warn!(
                                replica_id = member.id,
                                "reconfigure standby failed (will retry): {}",
                                e
                            );
                        }
                    }

                    // 2. Configure synchronous_standby_names
                    let result = configure_sync_standbys(
                        &self_instance,
                        &current.members,
                        current.write_quorum,
                    )
                    .await;
                    let _ = reply.send(result);
                }

                ReplicatorControlEvent::UpdateCurrentConfiguration { current, reply, .. } => {
                    // Finalize sync standby config (same logic)
                    let result = configure_sync_standbys(
                        &self_instance,
                        &current.members,
                        current.write_quorum,
                    )
                    .await;
                    let _ = reply.send(result);
                }

                ReplicatorControlEvent::WaitForCatchUpQuorum { reply, .. } => {
                    // Poll pg_stat_replication until all sync standbys
                    // have flush_lsn >= current WAL LSN
                    let inst = self_instance.clone();
                    tokio::spawn(async move {
                        let result = wait_for_catchup(&inst).await;
                        let _ = reply.send(result);
                    });
                }

                ReplicatorControlEvent::RemoveReplica {
                    replica_id, reply, ..
                } => {
                    tracing::info!(replica_id, "RemoveReplica");
                    // Replication slot cleanup could go here.
                    // synchronous_standby_names is updated by the next
                    // UpdateCurrentConfiguration call.
                    let _ = reply.send(Ok(()));
                }

                ReplicatorControlEvent::OnDataLoss { reply } => {
                    let _ = reply.send(Ok(DataLossAction::None));
                }

                ReplicatorControlEvent::Close { reply } => {
                    tracing::info!("PgReplicatorAdapter: Close — stopping PG");
                    if let Err(e) = self_instance.stop().await {
                        tracing::warn!("PG stop on close: {}", e);
                    }
                    if current_role == Role::None {
                        // Permanent removal — delete data directory
                        let dir = self_instance.data_dir().to_path_buf();
                        tracing::info!(?dir, "deleting data directory (decommissioned)");
                        let _ = tokio::fs::remove_dir_all(&dir).await;
                    }
                    shutdown_token.cancel();
                    let _ = reply.send(Ok(()));
                }

                ReplicatorControlEvent::Abort => {
                    tracing::info!("PgReplicatorAdapter: Abort — stopping PG");
                    if let Err(e) = self_instance.stop().await {
                        tracing::warn!("PG stop on abort: {}", e);
                    }
                    shutdown_token.cancel();
                }
            }
        }
    });

    ReplicatorHandle::new(control_tx, state, data_address, shutdown)
}

async fn handle_change_role(
    instance: &Arc<PgInstanceManager>,
    monitor: &Arc<PgMonitor>,
    role: Role,
) -> Result<()> {
    match role {
        Role::Primary => {
            // Only promote if currently in standby mode
            let (client, _conn) = instance.connect().await.map_err(|e| {
                kuberic_core::KubericError::Internal(format!("connect for role check: {e}").into())
            })?;
            let row = client
                .query_one("SELECT pg_is_in_recovery()", &[])
                .await
                .map_err(|e| {
                    kuberic_core::KubericError::Internal(format!("pg_is_in_recovery: {e}").into())
                })?;
            let in_recovery: bool = row.get(0);

            if in_recovery {
                tracing::info!("ChangeRole → Primary: promoting standby");
                instance.promote().await.map_err(|e| {
                    kuberic_core::KubericError::Internal(format!("promote: {e}").into())
                })?;
            } else {
                tracing::info!("ChangeRole → Primary: already primary");
            }
            monitor.set_role(Role::Primary);
            Ok(())
        }

        Role::IdleSecondary => {
            tracing::info!("ChangeRole → IdleSecondary");
            monitor.set_role(Role::IdleSecondary);
            // Reply immediately — BuildReplica arrives later
            Ok(())
        }

        Role::ActiveSecondary => {
            tracing::info!("ChangeRole → ActiveSecondary");
            monitor.set_role(Role::ActiveSecondary);
            Ok(())
        }

        Role::None | Role::Unknown => {
            tracing::info!("ChangeRole → None/Unknown: demotion (PG stopped on Close)");
            monitor.set_role(role);
            Ok(())
        }
    }
}

/// Reconfigure a secondary to stream from a new primary.
async fn reconfigure_secondary(
    secondary_addr: &str,
    primary_host: &str,
    primary_port: u32,
    replica_id: i64,
) -> Result<()> {
    use crate::proto::pg_data_service_client::PgDataServiceClient;

    let mut client = PgDataServiceClient::connect(secondary_addr.to_string())
        .await
        .map_err(|e| {
            kuberic_core::KubericError::Internal(
                format!("connect to secondary for reconfigure: {e}").into(),
            )
        })?;

    let resp = client
        .reconfigure_standby(crate::proto::ReconfigureStandbyRequest {
            primary_host: primary_host.to_string(),
            primary_port,
            replica_id,
        })
        .await
        .map_err(|e| {
            kuberic_core::KubericError::Internal(format!("ReconfigureStandby RPC: {e}").into())
        })?;

    let resp = resp.into_inner();
    if resp.success {
        Ok(())
    } else {
        Err(kuberic_core::KubericError::Internal(
            format!("ReconfigureStandby failed: {}", resp.error).into(),
        ))
    }
}

/// Configure synchronous_standby_names from the replica set members.
/// Uses naming convention: application_name = kuberic_{replica_id}
/// Quorum derived from write_quorum (subtract 1 for the primary itself).
async fn configure_sync_standbys(
    instance: &Arc<PgInstanceManager>,
    members: &[kuberic_core::types::ReplicaInfo],
    write_quorum: u32,
) -> Result<()> {
    let standby_names: Vec<String> = members
        .iter()
        .filter(|r| r.role == Role::ActiveSecondary || r.role == Role::IdleSecondary)
        .map(|r| format!("kuberic_{}", r.id))
        .collect();

    if standby_names.is_empty() {
        tracing::debug!("no sync standbys to configure");
        return Ok(());
    }

    let (client, _conn) = instance.connect().await.map_err(|e| {
        kuberic_core::KubericError::Internal(format!("connect for sync config: {e}").into())
    })?;

    // Quorum = write_quorum - 1 (subtract the primary itself), minimum 1.
    // E.g., 3-replica write_quorum=2 → ANY 1; 5-replica write_quorum=3 → ANY 2.
    // SAFETY: r.id is i64 — kuberic_{i64} cannot contain SQL-special characters.
    let quorum = write_quorum.saturating_sub(1).max(1);
    let names = standby_names.join(", ");
    let sql = format!("ALTER SYSTEM SET synchronous_standby_names = 'ANY {quorum} ({names})'");
    client.execute(&sql, &[]).await.map_err(|e| {
        kuberic_core::KubericError::Internal(format!("set synchronous_standby_names: {e}").into())
    })?;
    client
        .execute("SELECT pg_reload_conf()", &[])
        .await
        .map_err(|e| kuberic_core::KubericError::Internal(format!("pg_reload_conf: {e}").into()))?;

    tracing::info!(standbys = ?standby_names, "configured synchronous_standby_names");
    Ok(())
}

/// Wait for all sync standbys to catch up to current WAL LSN.
/// Polls pg_stat_replication every 500ms, timeout after 30s.
async fn wait_for_catchup(instance: &Arc<PgInstanceManager>) -> Result<()> {
    let (client, _conn) = instance.connect().await.map_err(|e| {
        kuberic_core::KubericError::Internal(format!("connect for catchup: {e}").into())
    })?;

    // Record target LSN
    let row = client
        .query_one("SELECT pg_current_wal_lsn()::text", &[])
        .await
        .map_err(|e| {
            kuberic_core::KubericError::Internal(format!("current_wal_lsn: {e}").into())
        })?;
    let target_str: &str = row.get(0);
    let target_lsn = crate::monitor::parse_pg_lsn(target_str)
        .map_err(|e| kuberic_core::KubericError::Internal(format!("parse LSN: {e}").into()))?;

    tracing::info!(target_lsn, "waiting for standbys to catch up");

    for _ in 0..60 {
        // Check if all sync standbys have flush_lsn >= target
        let rows = client
            .query(
                "SELECT application_name, flush_lsn::text \
                 FROM pg_stat_replication \
                 WHERE sync_state IN ('sync', 'quorum')",
                &[],
            )
            .await
            .map_err(|e| {
                kuberic_core::KubericError::Internal(format!("pg_stat_replication: {e}").into())
            })?;

        if rows.is_empty() {
            // No sync standbys yet — wait
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            continue;
        }

        let all_caught_up = rows.iter().all(|row| {
            let flush_str: Option<&str> = row.get(1);
            flush_str
                .and_then(|s| crate::monitor::parse_pg_lsn(s).ok())
                .is_some_and(|lsn| lsn >= target_lsn)
        });

        if all_caught_up {
            tracing::info!("all standbys caught up");
            return Ok(());
        }

        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }

    Err(kuberic_core::KubericError::Internal(
        "WaitForCatchUpQuorum timeout (30s)".into(),
    ))
}
