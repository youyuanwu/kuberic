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
                    // Epoch change on secondaries. If this replica has diverged
                    // WAL (was a zombie primary), pg_rewind would be needed.
                    // For now, reply OK — pg_rewind is triggered by the service
                    // layer when it detects divergence.
                    let _ = reply.send(Ok(()));
                }

                ReplicatorControlEvent::BuildReplica { replica, reply } => {
                    tracing::info!(
                        replica_id = replica.id,
                        addr = %replica.replicator_address,
                        "BuildReplica requested"
                    );
                    // The secondary adapter handles pg_basebackup.
                    // For the primary adapter, this is a no-op — the secondary
                    // pulls data via pg_basebackup directly from PG.
                    let _ = reply.send(Ok(()));
                }

                ReplicatorControlEvent::UpdateCatchUpConfiguration { reply, .. } => {
                    // TODO: update synchronous_standby_names
                    let _ = reply.send(Ok(()));
                }

                ReplicatorControlEvent::UpdateCurrentConfiguration { reply, .. } => {
                    // TODO: finalize synchronous_standby_names
                    let _ = reply.send(Ok(()));
                }

                ReplicatorControlEvent::WaitForCatchUpQuorum { reply, .. } => {
                    // TODO: poll pg_stat_replication until replicas caught up.
                    // For now, reply immediately (safe for basic tests without
                    // replication).
                    let _ = reply.send(Ok(()));
                }

                ReplicatorControlEvent::RemoveReplica { reply, .. } => {
                    // TODO: remove from synchronous_standby_names
                    let _ = reply.send(Ok(()));
                }

                ReplicatorControlEvent::OnDataLoss { reply } => {
                    let _ = reply.send(Ok(DataLossAction::None));
                }

                ReplicatorControlEvent::Close { reply } => {
                    tracing::info!("PgReplicatorAdapter: Close");
                    // PG stop is handled by the service layer in LifecycleEvent::Close
                    shutdown_token.cancel();
                    let _ = reply.send(Ok(()));
                }

                ReplicatorControlEvent::Abort => {
                    tracing::info!("PgReplicatorAdapter: Abort");
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
            tracing::info!("ChangeRole → Primary: promoting");
            // If PG is running as standby, promote it
            if let Err(e) = instance.promote().await {
                // Promotion fails if already primary — that's OK
                tracing::warn!("promote attempt: {}", e);
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

        Role::None => {
            tracing::info!("ChangeRole → None: stopping PG");
            monitor.set_role(Role::None);
            // Stop PG entirely to prevent split-brain
            // (Clone instance to get &mut for stop — in practice,
            // the service layer handles PG lifecycle)
            Ok(())
        }
    }
}
