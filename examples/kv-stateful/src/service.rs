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

                    // Cancel previous background tasks
                    if let Some(token) = bg_token.take() {
                        token.cancel();
                    }
                    for h in bg_handles.drain(..) {
                        let _ = h.await;
                    }

                    let token = CancellationToken::new();
                    bg_token = Some(token.clone());

                    let c = ctx.as_mut().unwrap();

                    match new_role {
                        Role::IdleSecondary => {
                            if let Some(copy) = c.copy_stream.take() {
                                let st = state.clone();
                                bg_handles.push(tokio::spawn(
                                    drain_stream(st, copy, "copy"),
                                ));
                            }
                        }
                        Role::ActiveSecondary => {
                            if let Some(repl) = c.replication_stream.take() {
                                let st = state.clone();
                                bg_handles.push(tokio::spawn(
                                    drain_stream(st, repl, "replication"),
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
                    info!(previous_epoch_last_lsn, "epoch updated");
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
                StateProviderEvent::GetCopyState { up_to_lsn, mut copy_context, reply } => {
                    let peer_lsn = if let Some(op) = copy_context.get_operation().await {
                        String::from_utf8_lossy(&op.data).parse::<i64>().unwrap_or(0)
                    } else {
                        0
                    };

                    info!(peer_lsn, up_to_lsn, "producing copy state");

                    let (tx, stream) = OperationStream::channel(64);
                    let st = state.read().await;
                    if peer_lsn < st.last_applied_lsn {
                        let current_lsn = st.last_applied_lsn;
                        for (k, v) in &st.data {
                            let op = KvOp::Put {
                                key: k.clone(),
                                value: v.clone(),
                            };
                            let data = Bytes::from(serde_json::to_vec(&op).unwrap());
                            if tx.send(Operation::new(current_lsn, data, None)).await.is_err() {
                                break;
                            }
                        }
                    }
                    drop(tx);
                    drop(st);
                    info!("copy state produced");
                    let _ = reply.send(Ok(stream));
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
