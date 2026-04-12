use bytes::Bytes;
use kuberic_core::events::{LifecycleEvent, StateProviderEvent};
use kuberic_core::handles::StateReplicatorHandle;
use kuberic_core::replicator::WalReplicator;
use kuberic_core::types::{CancellationToken, Operation, OperationStream, Role};
use tokio::sync::mpsc;
use tracing::info;

use crate::server::run_client_server;
use crate::state::{KvOp, SharedState, drain_stream};

/// Handle a single state provider event.
///
/// This is the KV service's "state provider" — the replicator calls these
/// during copy, catchup, and reconfiguration. Matches SF's IStateProvider.
async fn handle_state_provider_event(event: StateProviderEvent, state: &SharedState) {
    match event {
        StateProviderEvent::UpdateEpoch {
            previous_epoch_last_lsn,
            reply,
            ..
        } => {
            // A6: Rollback uncommitted ops on epoch change.
            let current_lsn = state.read().await.last_applied_lsn;
            if previous_epoch_last_lsn > 0 && previous_epoch_last_lsn < current_lsn {
                info!(
                    previous_epoch_last_lsn,
                    current_lsn, "epoch updated — rolling back uncommitted ops"
                );
                if let Err(e) = state
                    .write()
                    .await
                    .rollback_to(previous_epoch_last_lsn)
                    .await
                {
                    tracing::warn!(error = %e, "rollback failed");
                }
            } else {
                info!(previous_epoch_last_lsn, current_lsn, "epoch updated");
            }
            let _ = reply.send(Ok(()));
        }
        StateProviderEvent::GetLastCommittedLsn { reply } => {
            let lsn = state.read().await.last_applied_lsn;
            info!(lsn, "reporting last committed LSN");
            let _ = reply.send(Ok(lsn));
        }
        StateProviderEvent::GetCopyContext { reply } => {
            let lsn = state.read().await.last_applied_lsn;
            let (tx, stream) = OperationStream::channel(1);
            let data = Bytes::from(lsn.to_string());
            let _ = tx.send(Operation::new(0, data, None)).await;
            drop(tx);
            info!(lsn, "sent copy context");
            let _ = reply.send(Ok(stream));
        }
        StateProviderEvent::GetCopyState {
            up_to_lsn,
            mut copy_context,
            reply,
        } => {
            // Spawn to background — state serialization can be slow
            // for large datasets and must not block the event loop.
            let st = state.clone();
            tokio::spawn(async move {
                let peer_lsn = if let Some(op) = copy_context.get_operation().await {
                    String::from_utf8_lossy(&op.data)
                        .parse::<i64>()
                        .unwrap_or(0)
                } else {
                    0
                };

                info!(peer_lsn, up_to_lsn, "producing copy state");

                let (snapshot, current_lsn): (Vec<(String, String)>, i64) = {
                    let guard = st.read().await;
                    let lsn = guard.last_applied_lsn;
                    if peer_lsn < lsn {
                        let data: Vec<_> = guard
                            .data
                            .iter()
                            .map(|(k, v)| (k.clone(), v.clone()))
                            .collect();
                        (data, lsn)
                    } else {
                        (Vec::new(), lsn)
                    }
                };

                let (tx, stream) = OperationStream::channel(64);
                let _ = reply.send(Ok(stream));

                for (k, v) in snapshot {
                    let op = KvOp::Put { key: k, value: v };
                    let data = Bytes::from(serde_json::to_vec(&op).unwrap());
                    if tx
                        .send(Operation::new(current_lsn, data, None))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                drop(tx);
                info!("copy state produced");
            });
        }
        StateProviderEvent::OnDataLoss { reply } => {
            info!("data loss reported, accepting state as-is");
            let _ = reply.send(Ok(false));
        }
    }
}

/// Main service event loop. Processes lifecycle and state provider events
/// with biased select (lifecycle takes priority).
///
/// In the new API, the user creates the replicator in the Open handler
/// and returns a ReplicatorHandle to the runtime.
pub async fn run_service(
    mut lifecycle_rx: mpsc::Receiver<LifecycleEvent>,
    state: SharedState,
    client_bind: String,
) {
    let mut partition = None;
    let mut replicator: Option<StateReplicatorHandle> = None;
    let mut state_provider_rx: Option<mpsc::UnboundedReceiver<StateProviderEvent>> = None;
    let mut copy_stream: Option<OperationStream> = None;
    let mut replication_stream: Option<OperationStream> = None;
    let mut token: Option<CancellationToken> = None;
    let mut bg_handles: Vec<tokio::task::JoinHandle<()>> = Vec::new();
    let mut bg_token: Option<CancellationToken> = None;
    let mut client_server_handle: Option<tokio::task::JoinHandle<()>> = None;
    let mut client_server_shutdown: Option<CancellationToken> = None;

    info!("kv service started, waiting for events");

    loop {
        tokio::select! {
            biased;

            Some(event) = lifecycle_rx.recv() => match event {
                LifecycleEvent::Open { ctx, reply } => {
                    info!("service opened — creating replicator");
                    // User creates the state provider channel
                    let (sp_tx, sp_rx) = mpsc::unbounded_channel();
                    // User creates the replicator, passes state_provider_tx
                    match WalReplicator::create(
                        ctx.replica_id,
                        &ctx.data_bind,
                        ctx.fault_tx.clone(),
                        sp_tx,
                    ).await {
                        Ok((handle, handles)) => {
                            partition = Some(handles.partition);
                            replicator = Some(handles.replicator);
                            copy_stream = handles.copy_stream;
                            replication_stream = handles.replication_stream;
                            state_provider_rx = Some(sp_rx);
                            token = Some(ctx.token);
                            let _ = reply.send(Ok(handle));
                        }
                        Err(e) => {
                            tracing::error!(error = %e, "failed to create replicator");
                            let _ = reply.send(Err(e));
                        }
                    }
                }
                LifecycleEvent::ChangeRole { new_role, reply } => {
                    info!(?new_role, "role changed");

                    if new_role == Role::ActiveSecondary {
                        // IdleSecondary → ActiveSecondary: let copy drain finish
                        for h in bg_handles.drain(..) {
                            let _ = h.await;
                        }
                        // Checkpoint after copy completes
                        {
                            let mut guard = state.write().await;
                            guard.committed_lsn = guard.last_applied_lsn;
                            if let Err(e) = guard.checkpoint().await {
                                tracing::warn!(error = %e, "checkpoint after copy failed");
                            }
                        }
                    } else {
                        if let Some(t) = bg_token.take() {
                            t.cancel();
                        }
                        for h in bg_handles.drain(..) {
                            let _ = h.await;
                        }
                    }

                    let t = CancellationToken::new();
                    bg_token = Some(t.clone());

                    match new_role {
                        Role::IdleSecondary => {
                            if let Some(cs) = copy_stream.take() {
                                let st = state.clone();
                                bg_handles.push(tokio::spawn(
                                    drain_stream(st, cs, t.clone(), "copy"),
                                ));
                            }
                        }
                        Role::ActiveSecondary => {
                            if let Some(rs) = replication_stream.take() {
                                let st = state.clone();
                                bg_handles.push(tokio::spawn(
                                    drain_stream(st, rs, t.clone(), "replication"),
                                ));
                            }
                        }
                        Role::Primary => {
                            if client_server_handle.is_none() {
                                let srv_state = state.clone();
                                let p = partition.as_ref().unwrap().clone();
                                let r = replicator.as_ref().unwrap().clone();
                                let srv_token = token.as_ref().unwrap().clone();
                                let shutdown = CancellationToken::new();
                                let shutdown_cp = shutdown.clone();
                                let bind = client_bind.clone();

                                client_server_shutdown = Some(shutdown);
                                client_server_handle = Some(tokio::spawn(async move {
                                    run_client_server(
                                        bind, srv_state, p, r,
                                        srv_token, shutdown_cp,
                                    ).await;
                                }));
                            }
                        }
                        Role::None => {}
                    }

                    let _ = reply.send(Ok(String::new()));
                }
                LifecycleEvent::Close { reply } => {
                    info!("service closing");
                    if let Some(token) = bg_token.take() {
                        token.cancel();
                    }
                    for h in bg_handles.drain(..) {
                        let _ = h.await;
                    }
                    if let Some(shutdown) = client_server_shutdown.take() {
                        shutdown.cancel();
                    }
                    if let Some(h) = client_server_handle.take() {
                        let _ = h.await;
                    }
                    // Checkpoint on graceful close for fast recovery
                    {
                        let mut guard = state.write().await;
                        guard.committed_lsn = guard.last_applied_lsn;
                        if let Err(e) = guard.checkpoint().await {
                            tracing::warn!(error = %e, "checkpoint on close failed");
                        }
                    }
                    let _ = reply.send(Ok(()));
                    break;
                }
                LifecycleEvent::Abort => {
                    if let Some(token) = bg_token.take() {
                        token.cancel();
                    }
                    if let Some(shutdown) = client_server_shutdown.take() {
                        shutdown.cancel();
                    }
                    break;
                }
            },

            Some(event) = async {
                match state_provider_rx.as_mut() {
                    Some(rx) => rx.recv().await,
                    None => std::future::pending().await,
                }
            } => {
                handle_state_provider_event(event, &state).await;
            },

            else => break,
        }
    }
    info!("kv service exited");
}
