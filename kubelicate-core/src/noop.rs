use std::sync::Arc;

use tokio::sync::mpsc;

use crate::events::{ReplicateRequest, ReplicatorControlEvent};
use crate::handles::PartitionState;
use crate::types::{Lsn, Role};

/// A no-op replicator for testing. Accepts all control events and returns Ok.
/// Replicate requests are assigned sequential LSNs and immediately acknowledged.
///
/// Note: The replicator does NOT set read/write access status — that is the
/// runtime's responsibility. The replicator only updates LSN progress values
/// on PartitionState.
pub struct NoopReplicator;

impl NoopReplicator {
    /// Run the noop replicator actor. Processes control and data channels
    /// until both are closed or an Abort/Close event is received.
    pub async fn run(
        mut control_rx: mpsc::Receiver<ReplicatorControlEvent>,
        mut data_rx: mpsc::Receiver<ReplicateRequest>,
        state: Arc<PartitionState>,
    ) {
        let mut role = Role::None;
        let mut next_lsn: Lsn = 1;

        loop {
            tokio::select! {
                biased;

                event = control_rx.recv() => {
                    let Some(event) = event else { break };
                    match event {
                        ReplicatorControlEvent::Open { reply, .. } => {
                            let _ = reply.send(Ok(()));
                        }
                        ReplicatorControlEvent::Close { reply } => {
                            let _ = reply.send(Ok(()));
                            break;
                        }
                        ReplicatorControlEvent::Abort => {
                            break;
                        }
                        ReplicatorControlEvent::ChangeRole {
                            role: new_role,
                            reply,
                            ..
                        } => {
                            role = new_role;
                            let _ = reply.send(Ok(()));
                        }
                        ReplicatorControlEvent::UpdateEpoch { reply, .. } => {
                            let _ = reply.send(Ok(()));
                        }
                        ReplicatorControlEvent::OnDataLoss { reply } => {
                            let _ = reply.send(Ok(crate::types::DataLossAction::None));
                        }
                        ReplicatorControlEvent::UpdateCatchUpConfiguration { reply, .. } => {
                            let _ = reply.send(Ok(()));
                        }
                        ReplicatorControlEvent::UpdateCurrentConfiguration { reply, .. } => {
                            let _ = reply.send(Ok(()));
                        }
                        ReplicatorControlEvent::WaitForCatchUpQuorum { reply, .. } => {
                            let _ = reply.send(Ok(()));
                        }
                        ReplicatorControlEvent::BuildReplica { reply, .. } => {
                            let _ = reply.send(Ok(()));
                        }
                        ReplicatorControlEvent::RemoveReplica { reply, .. } => {
                            let _ = reply.send(Ok(()));
                        }
                    }
                }

                req = data_rx.recv(), if role == Role::Primary => {
                    let Some(req) = req else { break };
                    let lsn = next_lsn;
                    next_lsn += 1;
                    state.set_current_progress(lsn);
                    state.set_committed_lsn(lsn);
                    let _ = req.reply.send(Ok(lsn));
                }

                else => break,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::ReplicateRequest;
    use crate::types::{AccessStatus, CancellationToken, Epoch, OpenMode};

    /// Simulates what the runtime does: sets access status around replicator calls.
    fn runtime_set_status_for_role(state: &PartitionState, role: Role) {
        match role {
            Role::Primary => {
                state.set_read_status(AccessStatus::Granted);
                state.set_write_status(AccessStatus::Granted);
            }
            _ => {
                state.set_read_status(AccessStatus::NotPrimary);
                state.set_write_status(AccessStatus::NotPrimary);
            }
        }
    }

    #[tokio::test]
    async fn test_noop_lifecycle() {
        let (control_tx, control_rx) = tokio::sync::mpsc::channel(16);
        let (data_tx, data_rx) = tokio::sync::mpsc::channel::<ReplicateRequest>(16);
        let state = Arc::new(PartitionState::new());

        let state_cp = state.clone();
        let handle = tokio::spawn(async move {
            NoopReplicator::run(control_rx, data_rx, state_cp).await;
        });

        // Open
        let (tx, rx) = tokio::sync::oneshot::channel();
        control_tx
            .send(ReplicatorControlEvent::Open {
                mode: OpenMode::New,
                reply: tx,
            })
            .await
            .unwrap();
        rx.await.unwrap().unwrap();

        // ChangeRole to Primary
        let (tx, rx) = tokio::sync::oneshot::channel();
        control_tx
            .send(ReplicatorControlEvent::ChangeRole {
                epoch: Epoch::new(0, 1),
                role: Role::Primary,
                reply: tx,
            })
            .await
            .unwrap();
        rx.await.unwrap().unwrap();

        // Runtime sets status after replicator confirms role change
        runtime_set_status_for_role(&state, Role::Primary);

        assert_eq!(state.read_status(), AccessStatus::Granted);
        assert_eq!(state.write_status(), AccessStatus::Granted);

        // Replicate
        let (tx, rx) = tokio::sync::oneshot::channel();
        data_tx
            .send(ReplicateRequest {
                data: bytes::Bytes::from("hello"),
                reply: tx,
            })
            .await
            .unwrap();
        let lsn = rx.await.unwrap().unwrap();
        assert_eq!(lsn, 1);
        assert_eq!(state.current_progress(), 1);

        // Replicate again
        let (tx, rx) = tokio::sync::oneshot::channel();
        data_tx
            .send(ReplicateRequest {
                data: bytes::Bytes::from("world"),
                reply: tx,
            })
            .await
            .unwrap();
        let lsn = rx.await.unwrap().unwrap();
        assert_eq!(lsn, 2);

        // ChangeRole to Secondary
        let (tx, rx) = tokio::sync::oneshot::channel();
        control_tx
            .send(ReplicatorControlEvent::ChangeRole {
                epoch: Epoch::new(0, 2),
                role: Role::ActiveSecondary,
                reply: tx,
            })
            .await
            .unwrap();
        rx.await.unwrap().unwrap();

        // Runtime sets status after replicator confirms role change
        runtime_set_status_for_role(&state, Role::ActiveSecondary);

        assert_eq!(state.read_status(), AccessStatus::NotPrimary);
        assert_eq!(state.write_status(), AccessStatus::NotPrimary);

        // Close
        let (tx, rx) = tokio::sync::oneshot::channel();
        control_tx
            .send(ReplicatorControlEvent::Close { reply: tx })
            .await
            .unwrap();
        rx.await.unwrap().unwrap();

        handle.await.unwrap();
    }

    #[tokio::test]
    async fn test_noop_replicate_handle() {
        use crate::handles::StateReplicatorHandle;

        let (control_tx, control_rx) = tokio::sync::mpsc::channel(16);
        let (data_tx, data_rx) = tokio::sync::mpsc::channel::<ReplicateRequest>(16);
        let state = Arc::new(PartitionState::new());

        let replicator_handle = StateReplicatorHandle::new(data_tx.clone(), state.clone());

        let state_cp = state.clone();
        let handle = tokio::spawn(async move {
            NoopReplicator::run(control_rx, data_rx, state_cp).await;
        });

        // Open + ChangeRole(Primary)
        let (tx, rx) = tokio::sync::oneshot::channel();
        control_tx
            .send(ReplicatorControlEvent::Open {
                mode: OpenMode::New,
                reply: tx,
            })
            .await
            .unwrap();
        rx.await.unwrap().unwrap();

        let (tx, rx) = tokio::sync::oneshot::channel();
        control_tx
            .send(ReplicatorControlEvent::ChangeRole {
                epoch: Epoch::new(0, 1),
                role: Role::Primary,
                reply: tx,
            })
            .await
            .unwrap();
        rx.await.unwrap().unwrap();

        // Runtime sets status after replicator confirms role change
        runtime_set_status_for_role(&state, Role::Primary);

        // Replicate via handle
        let token = CancellationToken::new();
        let lsn = replicator_handle
            .replicate(bytes::Bytes::from("test"), token)
            .await
            .unwrap();
        assert_eq!(lsn, 1);

        // Close
        let (tx, rx) = tokio::sync::oneshot::channel();
        control_tx
            .send(ReplicatorControlEvent::Close { reply: tx })
            .await
            .unwrap();
        rx.await.unwrap().unwrap();

        handle.await.unwrap();
    }

    #[tokio::test]
    async fn test_noop_replicate_not_primary() {
        use crate::handles::StateReplicatorHandle;

        let (_control_tx, control_rx) = tokio::sync::mpsc::channel(16);
        let (data_tx, data_rx) = tokio::sync::mpsc::channel::<ReplicateRequest>(16);
        let state = Arc::new(PartitionState::new());

        let replicator_handle = StateReplicatorHandle::new(data_tx.clone(), state.clone());

        let state_cp = state.clone();
        let _handle = tokio::spawn(async move {
            NoopReplicator::run(control_rx, data_rx, state_cp).await;
        });

        // Don't promote to primary — status is NotPrimary
        let token = CancellationToken::new();
        let result = replicator_handle
            .replicate(bytes::Bytes::from("test"), token)
            .await;

        assert!(matches!(result, Err(crate::KubelicateError::NotPrimary)));
    }
}
