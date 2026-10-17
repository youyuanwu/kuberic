use crate::protocol::types::OperationId;
use bytes::Bytes;
use std::{future::Future, sync::Arc};
use tokio::sync::{Mutex, mpsc, oneshot, watch};

use crate::application::{DurableApplicationProgress, Lsn};
use crate::{Result, RuntimeError};

/// Identifies the kind of operation and its replication or copy progress.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OperationMetadata {
    Replication {
        lsn: Lsn,
        committed_lsn: Lsn,
    },
    Copy {
        build_id: OperationId,
        sequence: u64,
    },
    CopyComplete {
        build_id: OperationId,
        up_to_lsn: Lsn,
        committed_lsn: Lsn,
    },
}

/// A copy or replication operation supplied by the primary for secondary
/// application.
///
/// Acknowledge only after the operation and its progress are durably accepted.
/// Dropping an operation is not an ACK and fails the waiting data-plane request.
/// Every successfully applied stream operation requires an explicit
/// acknowledgement.
pub struct StreamOperation {
    /// The operation kind and the progress associated with its payload.
    pub metadata: OperationMetadata,
    /// Opaque operation data supplied by the primary.
    pub data: Bytes,
    completion: oneshot::Sender<Result<DurableApplicationProgress>>,
    cancelled: watch::Receiver<bool>,
    closed: watch::Receiver<bool>,
    effects: Arc<Mutex<()>>,
}

pub(crate) struct OperationCompletion {
    receiver: oneshot::Receiver<Result<DurableApplicationProgress>>,
    closed: watch::Receiver<bool>,
    cancelled: watch::Sender<bool>,
}

impl Drop for OperationCompletion {
    fn drop(&mut self) {
        self.cancelled.send_replace(true);
    }
}

impl OperationCompletion {
    pub(crate) async fn completed(mut self) -> Result<DurableApplicationProgress> {
        tokio::select! {
            biased;
            _ = self.closed.changed() => Err(RuntimeError::Closed),
            result = &mut self.receiver => result.map_err(|_| RuntimeError::WriteCompletionClosed)?,
        }
    }
}

impl StreamOperation {
    /// Explicit service-owned adapter for the built-in durable state contract.
    /// It verifies retained duplicate bytes without applying them twice, and
    /// commits the requested watermark before returning progress. The service
    /// must still explicitly acknowledge or reject this result.
    pub async fn apply_to(
        &self,
        storage: &dyn crate::engine::DurableState,
    ) -> Result<DurableApplicationProgress> {
        self.execute(async {
            match &self.metadata {
                OperationMetadata::Replication { lsn, committed_lsn } => {
                    let prior = storage.durable_progress().await?;
                    if *lsn > prior.applied_lsn {
                        if *lsn != prior.applied_lsn.saturating_add(1) {
                            return Err(RuntimeError::Application(
                                "stream operation has a gap".into(),
                            ));
                        }
                        storage
                            .apply(crate::application::Operation {
                                lsn: *lsn,
                                committed_lsn: prior.committed_lsn,
                                data: self.data.clone(),
                            })
                            .await?;
                    } else {
                        use futures::StreamExt;
                        let mut retained = storage.get_replication_operations(*lsn, *lsn).await?;
                        let existing = retained.next().await.transpose()?;
                        if existing.as_ref().is_none_or(|operation| {
                            operation.lsn != *lsn || operation.data != self.data
                        }) || retained.next().await.is_some()
                        {
                            return Err(RuntimeError::Application(
                                "stream duplicate changed bytes".into(),
                            ));
                        }
                    }
                    if *committed_lsn > prior.committed_lsn {
                        storage.commit(*committed_lsn).await
                    } else {
                        storage.durable_progress().await
                    }
                }
                OperationMetadata::Copy { build_id, sequence } => {
                    storage
                        .apply_copy_chunk(
                            build_id,
                            *sequence,
                            crate::application::CopyChunk {
                                data: self.data.clone(),
                            },
                        )
                        .await?;
                    storage.durable_progress().await
                }
                OperationMetadata::CopyComplete {
                    build_id,
                    up_to_lsn,
                    committed_lsn,
                } => {
                    storage
                        .finish_copy(build_id, *up_to_lsn, *committed_lsn)
                        .await
                }
            }
        })
        .await
    }

    /// Runs service persistence within this delivery's lifetime. Use this for
    /// application effects before explicitly acknowledging or rejecting them.
    /// Losing the request or authority cancels the future; already durable
    /// effects are not rolled back. Persistence must be cancellation-safe and
    /// must not detach work that outlives this future.
    pub async fn execute<T>(&self, effect: impl Future<Output = Result<T>>) -> Result<T> {
        let mut cancelled = self.cancelled.clone();
        let mut closed = self.closed.clone();
        if *cancelled.borrow() || *closed.borrow() {
            return Err(RuntimeError::OperationCancelled);
        }
        tokio::select! {
            biased;
            _ = cancelled.changed() => Err(RuntimeError::OperationCancelled),
            _ = closed.changed() => Err(RuntimeError::Closed),
            result = async {
                let _active = self.effects.lock().await;
                // The cancellation branches also cover waiting for this gate.
                effect.await
            } => result,
        }
    }

    /// Acknowledges successful durable application to the secondary's local
    /// store, including the supplied applied and committed progress.
    ///
    /// The sender waits for this acknowledgement. It is a local completion,
    /// not an independent quorum decision; the replicator aggregates replica
    /// acknowledgements before completing a primary replication call.
    pub fn acknowledge(self, progress: DurableApplicationProgress) -> Result<()> {
        if *self.cancelled.borrow() || *self.closed.borrow() {
            return Err(RuntimeError::OperationCancelled);
        }
        self.completion
            .send(Ok(progress))
            .map_err(|_| RuntimeError::Closed)
    }

    /// Reports an application failure to the waiting sender instead of
    /// acknowledging the operation.
    pub fn reject(self, error: RuntimeError) -> Result<()> {
        self.completion
            .send(Err(error))
            .map_err(|_| RuntimeError::Closed)
    }
}

/// A stream of copy or replication operations sent from primary to secondary.
///
/// Each delivered [`StreamOperation`] carries metadata and opaque data and must
/// be applied and explicitly acknowledged, or rejected on application failure.
pub struct OperationStream {
    receiver: mpsc::Receiver<StreamOperation>,
    closed: watch::Receiver<bool>,
}

impl OperationStream {
    pub fn channel(capacity: usize) -> (OperationSender, Self) {
        Self::channel_with_effects(capacity, Arc::new(Mutex::new(())))
    }

    fn channel_with_effects(capacity: usize, effects: Arc<Mutex<()>>) -> (OperationSender, Self) {
        let (sender, receiver) = mpsc::channel(capacity);
        let (closed, closed_rx) = watch::channel(false);
        (
            OperationSender {
                sender,
                closed,
                effects,
            },
            Self {
                receiver,
                closed: closed_rx,
            },
        )
    }

    /// Waits for the next copy or replication operation.
    ///
    /// Returns `Some` for a delivered operation and `None` when the stream is
    /// closed or has ended.
    /// A temporarily empty open stream waits rather than reporting completion.
    pub async fn get_operation(&mut self) -> Result<Option<StreamOperation>> {
        if *self.closed.borrow() {
            return Ok(None);
        }
        tokio::select! {
            biased;
            _ = self.closed.changed() => Ok(None),
            operation = self.receiver.recv() => Ok(operation),
        }
    }
}

/// Producer side for replication engines implementing service delivery.
#[derive(Clone)]
pub struct OperationSender {
    sender: mpsc::Sender<StreamOperation>,
    closed: watch::Sender<bool>,
    effects: Arc<Mutex<()>>,
}

impl OperationSender {
    pub fn close(&self) {
        self.closed.send_replace(true);
    }

    pub(crate) async fn enqueue(
        &self,
        metadata: OperationMetadata,
        data: Bytes,
    ) -> Result<OperationCompletion> {
        let mut closed = self.closed.subscribe();
        if *closed.borrow() {
            return Err(RuntimeError::Closed);
        }
        let (completion, receiver) = oneshot::channel();
        let (cancelled, cancellation) = watch::channel(false);
        let pending = OperationCompletion {
            receiver,
            closed: closed.clone(),
            cancelled,
        };
        let operation = StreamOperation {
            metadata,
            data,
            completion,
            cancelled: cancellation,
            closed: closed.clone(),
            effects: self.effects.clone(),
        };
        tokio::select! {
            biased;
            _ = closed.changed() => Err(RuntimeError::Closed),
            result = self.sender.send(operation) => {
                result.map_err(|_| RuntimeError::Closed)?;
                Ok(pending)
            },
        }
    }

    /// Delivers an operation and waits for the recipient's durable application
    /// acknowledgement or rejection. This local stream completion is not a
    /// quorum decision.
    pub async fn send(
        &self,
        metadata: OperationMetadata,
        data: Bytes,
    ) -> Result<DurableApplicationProgress> {
        self.enqueue(metadata, data).await?.completed().await
    }
}

pub(crate) struct ServiceStreams {
    replication_tx: OperationSender,
    copy_tx: OperationSender,
    replication: Mutex<Option<OperationStream>>,
    copy: Mutex<Option<OperationStream>>,
    effects: Arc<Mutex<()>>,
}

impl ServiceStreams {
    pub(crate) fn new() -> Self {
        let effects = Arc::new(Mutex::new(()));
        let (replication_tx, replication) =
            OperationStream::channel_with_effects(64, effects.clone());
        let (copy_tx, copy) = OperationStream::channel_with_effects(64, effects.clone());
        Self {
            replication_tx,
            copy_tx,
            replication: Mutex::new(Some(replication)),
            copy: Mutex::new(Some(copy)),
            effects,
        }
    }

    pub(crate) async fn drain(&self) {
        let _drained = self.effects.lock().await;
    }

    pub(crate) async fn deliver(
        &self,
        metadata: OperationMetadata,
        data: Bytes,
    ) -> Result<DurableApplicationProgress> {
        match metadata {
            OperationMetadata::Replication { .. } => self.replication_tx.send(metadata, data).await,
            _ => self.copy_tx.send(metadata, data).await,
        }
    }

    pub(crate) fn shutdown(&self) {
        self.replication_tx.close();
        self.copy_tx.close();
    }

    pub(crate) async fn take_replication(&self) -> Result<OperationStream> {
        self.replication
            .lock()
            .await
            .take()
            .ok_or_else(|| RuntimeError::Application("replication stream already taken".into()))
    }

    pub(crate) async fn take_copy(&self) -> Result<OperationStream> {
        self.copy
            .lock()
            .await
            .take()
            .ok_or_else(|| RuntimeError::Application("copy stream already taken".into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    fn metadata() -> OperationMetadata {
        OperationMetadata::Replication {
            lsn: 1,
            committed_lsn: 0,
        }
    }

    #[tokio::test]
    async fn stream_cancelled_queue_cannot_start_persistence() {
        let (sender, mut stream) = OperationStream::channel(1);
        let completion = sender.enqueue(metadata(), Bytes::new()).await.unwrap();
        drop(completion);
        let operation = stream.get_operation().await.unwrap().unwrap();
        let mutated = AtomicBool::new(false);
        assert!(
            operation
                .execute(async {
                    mutated.store(true, Ordering::Release);
                    Ok(())
                })
                .await
                .is_err()
        );
        assert!(!mutated.load(Ordering::Acquire));
        assert!(
            operation
                .acknowledge(DurableApplicationProgress::default())
                .is_err()
        );
    }

    #[tokio::test]
    async fn stream_request_cancellation_drains_active_effect() {
        struct Dropped(Arc<AtomicBool>);
        impl Drop for Dropped {
            fn drop(&mut self) {
                self.0.store(true, Ordering::Release);
            }
        }
        let streams = Arc::new(ServiceStreams::new());
        let mut stream = streams.take_replication().await.unwrap();
        let request = tokio::spawn({
            let streams = streams.clone();
            async move { streams.deliver(metadata(), Bytes::new()).await }
        });
        let operation = stream.get_operation().await.unwrap().unwrap();
        let dropped = Arc::new(AtomicBool::new(false));
        let probe = dropped.clone();
        let (entered, started) = oneshot::channel();
        let consumer = tokio::spawn(async move {
            operation
                .execute(async move {
                    let _probe = Dropped(probe);
                    entered.send(()).unwrap();
                    std::future::pending::<Result<()>>().await
                })
                .await
        });
        started.await.unwrap();
        request.abort();
        let _ = request.await;
        tokio::time::timeout(Duration::from_secs(2), streams.drain())
            .await
            .unwrap();
        assert!(dropped.load(Ordering::Acquire));
        assert!(consumer.await.unwrap().is_err());
    }

    #[tokio::test]
    async fn stream_shutdown_cancels_backpressure_and_terminal_reads() {
        let (sender, mut stream) = OperationStream::channel(1);
        let first = sender.enqueue(metadata(), Bytes::new()).await.unwrap();
        let blocked = sender.send(metadata(), Bytes::new());
        tokio::pin!(blocked);
        assert!(
            tokio::time::timeout(Duration::from_millis(20), &mut blocked)
                .await
                .is_err()
        );
        sender.close();
        assert!(first.completed().await.is_err());
        assert!(blocked.await.is_err());
        assert!(stream.get_operation().await.unwrap().is_none());
    }
}
