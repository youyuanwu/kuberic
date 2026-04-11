use bytes::Bytes;
use kubelicate_core::events::{LifecycleEvent, ServiceContext, StateProviderEvent};
use kubelicate_core::types::{CancellationToken, Operation, OperationStream, Role};
use tokio::sync::mpsc;
use tracing::info;

use crate::server::run_client_server;
use crate::state::{KvOp, SharedState, drain_stream};

/// Main service event loop. Processes lifecycle and state provider events
/// with biased select (lifecycle takes priority).
pub async fn run_service(
    mut lifecycle_rx: mpsc::Receiver<LifecycleEvent>,
    mut state_provider_rx: mpsc::Receiver<StateProviderEvent>,
    state: SharedState,
    client_bind: String,
) {
    let mut ctx: Option<ServiceContext> = None;
    let mut bg_handles: Vec<tokio::task::JoinHandle<()>> = Vec::new();
    let mut bg_token: Option<CancellationToken> = None;
    let mut client_server_handle: Option<tokio::task::JoinHandle<()>> = None;
    let mut client_server_shutdown: Option<CancellationToken> = None;

    info!("kv service started, waiting for events");

    loop {
        tokio::select! {
            biased;

            Some(event) = lifecycle_rx.recv() => match event {
                LifecycleEvent::Open { ctx: c, reply } => {
                    info!("service opened");
                    ctx = Some(c);
                    let _ = reply.send(Ok(()));
                }
                LifecycleEvent::ChangeRole { new_role, reply } => {
                    info!(?new_role, "role changed");

                    if new_role == Role::ActiveSecondary {
                        // IdleSecondary → ActiveSecondary: let copy drain finish
                        // naturally without cancelling. The copy stream is already
                        // closed (sender dropped by gRPC handler), so the drain
                        // completes once remaining items are consumed. This ensures
                        // no copy items are lost — critical for non-idempotent ops.
                        for h in bg_handles.drain(..) {
                            let _ = h.await;
                        }
                        // Checkpoint after copy completes — all copied state is committed
                        {
                            let mut guard = state.write().await;
                            guard.committed_lsn = guard.last_applied_lsn;
                            if let Err(e) = guard.checkpoint().await {
                                tracing::warn!(error = %e, "checkpoint after copy failed");
                            }
                        }
                    } else {
                        // Cancel previous background tasks for other transitions
                        if let Some(token) = bg_token.take() {
                            token.cancel();
                        }
                        for h in bg_handles.drain(..) {
                            let _ = h.await;
                        }
                    }

                    let token = CancellationToken::new();
                    bg_token = Some(token.clone());

                    let c = ctx.as_mut().unwrap();

                    match new_role {
                        Role::IdleSecondary => {
                            if let Some(copy) = c.copy_stream.take() {
                                let st = state.clone();
                                let t = token.clone();
                                bg_handles.push(tokio::spawn(
                                    drain_stream(st, copy, t, "copy"),
                                ));
                            }
                        }
                        Role::ActiveSecondary => {
                            if let Some(repl) = c.replication_stream.take() {
                                let st = state.clone();
                                let t = token.clone();
                                bg_handles.push(tokio::spawn(
                                    drain_stream(st, repl, t, "replication"),
                                ));
                            }
                        }
                        Role::Primary => {
                            if client_server_handle.is_none() {
                                let srv_state = state.clone();
                                let partition = c.partition.clone();
                                let replicator = c.replicator.clone();
                                let srv_token = c.token.clone();
                                let shutdown = CancellationToken::new();
                                let shutdown_cp = shutdown.clone();
                                let bind = client_bind.clone();

                                client_server_shutdown = Some(shutdown);
                                client_server_handle = Some(tokio::spawn(async move {
                                    run_client_server(
                                        bind, srv_state, partition, replicator,
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

            Some(event) = state_provider_rx.recv() => match event {
                StateProviderEvent::UpdateEpoch { previous_epoch_last_lsn, reply, .. } => {
                    // For now, log epoch changes without rollback.
                    // Full rollback (A6 fix) requires refactoring to avoid
                    // holding the write lock across async file I/O.
                    // See design-gaps.md A6 for the planned approach.
                    info!(previous_epoch_last_lsn, "epoch updated");
                    let _ = reply.send(Ok(()));
                }
                StateProviderEvent::GetLastCommittedLsn { reply } => {
                    let lsn = state.read().await.last_applied_lsn;
                    info!(lsn, "reporting last committed LSN");
                    let _ = reply.send(Ok(lsn));
                }
                StateProviderEvent::GetCopyContext { reply } => {
                    // Fast — just send current LSN. Safe to run inline.
                    let lsn = state.read().await.last_applied_lsn;
                    let (tx, stream) = OperationStream::channel(1);
                    let data = Bytes::from(lsn.to_string());
                    let _ = tx.send(Operation::new(0, data, None)).await;
                    drop(tx);
                    info!(lsn, "sent copy context");
                    let _ = reply.send(Ok(stream));
                }
                StateProviderEvent::GetCopyState { up_to_lsn, mut copy_context, reply } => {
                    // Spawn to background — state serialization can be slow
                    // for large datasets and must not block the event loop.
                    let st = state.clone();
                    tokio::spawn(async move {
                        let peer_lsn = if let Some(op) = copy_context.get_operation().await {
                            String::from_utf8_lossy(&op.data).parse::<i64>().unwrap_or(0)
                        } else {
                            0
                        };

                        info!(peer_lsn, up_to_lsn, "producing copy state");

                        // Collect state snapshot under read lock, then drop
                        // the lock BEFORE sending. This avoids deadlock: the
                        // drain task needs write lock to apply replication ops,
                        // and tx.send() can block if the receiver is slow.
                        let (snapshot, current_lsn): (Vec<(String, String)>, i64) = {
                            let guard = st.read().await;
                            let lsn = guard.last_applied_lsn;
                            if peer_lsn < lsn {
                                let data: Vec<_> = guard.data.iter()
                                    .map(|(k, v)| (k.clone(), v.clone()))
                                    .collect();
                                (data, lsn)
                            } else {
                                (Vec::new(), lsn)
                            }
                        };

                        let (tx, stream) = OperationStream::channel(64);
                        // Reply with the stream immediately
                        let _ = reply.send(Ok(stream));

                        for (k, v) in snapshot {
                            let op = KvOp::Put { key: k, value: v };
                            let data = Bytes::from(serde_json::to_vec(&op).unwrap());
                            if tx.send(Operation::new(current_lsn, data, None)).await.is_err() {
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
            },

            else => break,
        }
    }
    info!("kv service exited");
}
