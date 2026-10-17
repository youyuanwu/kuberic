use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use kuberic_runtime::application::{OpenContext, RoleChange, StatefulServiceReplica};
use kuberic_runtime::protocol::types::ReplicaRole;
use kuberic_runtime::replicator::stream::OperationStream;
use kuberic_runtime::replicator::{
    DefaultReplicatorFactory, Replicator, ReplicatorFactory, ReplicatorSettings, StateReplicator,
    StatefulServicePartition,
};
use kuberic_runtime::{Result, RuntimeError};
use tokio::sync::{Mutex as AsyncMutex, RwLock, watch};
use tokio::task::JoinSet;

use crate::persistence::KvPersistence;
use crate::state::{CopyGate, KvStateProvider};

pub struct KvService {
    provider: Arc<KvStateProvider>,
    replication_address: String,
    state_replicator: RwLock<Option<Arc<dyn StateReplicator>>>,
    control: RwLock<Option<Arc<dyn Replicator>>>,
    partition: RwLock<Option<StatefulServicePartition>>,
    role: Mutex<ReplicaRole>,
    stream_stop: watch::Sender<bool>,
    streams: AsyncMutex<StreamConsumers>,
}

#[derive(Default)]
struct StreamConsumers {
    tasks: JoinSet<Result<()>>,
    errors: Vec<String>,
}

impl KvService {
    pub fn new(
        persistence: Arc<KvPersistence>,
        replication_address: String,
        copy_gate: CopyGate,
    ) -> Self {
        Self {
            provider: Arc::new(KvStateProvider::new(persistence, copy_gate)),
            replication_address,
            state_replicator: RwLock::new(None),
            control: RwLock::new(None),
            partition: RwLock::new(None),
            role: Mutex::new(ReplicaRole::None),
            stream_stop: watch::channel(false).0,
            streams: AsyncMutex::new(StreamConsumers::default()),
        }
    }

    pub fn persistence(&self) -> &Arc<KvPersistence> {
        self.provider.persistence()
    }

    pub async fn replicate_put(&self, key: String, value: String) -> Result<i64> {
        let data = KvPersistence::encode_put(key, value)?;
        self.state_replicator
            .read()
            .await
            .as_ref()
            .ok_or(RuntimeError::NotOpen)?
            .replicate(data)
            .await
    }

    pub async fn get(&self, key: &str) -> Result<Option<String>> {
        let partition = self
            .partition
            .read()
            .await
            .clone()
            .ok_or(RuntimeError::NotOpen)?;
        let status = partition.get_read_status().await?;
        require_read_access(status)?;
        Ok(self.persistence().get(key))
    }

    /// Stop and await this instance's operation-stream consumers.
    ///
    /// Call after stopping the replica host/replicator. `close` and `abort` request
    /// cancellation without discarding completion ownership; this method remains
    /// usable afterward. It is terminal, repeatable, and cancellation-safe.
    /// Unexpected consumer errors or panics are retained and returned after all
    /// consumers terminate. Provider operation errors still reject their operation.
    pub async fn shutdown_and_drain(&self) -> Result<()> {
        self.close().await?;
        let mut streams = self.streams.lock().await;
        while let Some(result) = streams.tasks.join_next().await {
            match result {
                Ok(Ok(())) => {}
                Ok(Err(error)) => streams.errors.push(error.to_string()),
                Err(error) => streams
                    .errors
                    .push(format!("KVStore2 stream consumer task failed: {error}")),
            }
        }
        if streams.errors.is_empty() {
            Ok(())
        } else {
            Err(RuntimeError::Application(streams.errors.join("; ")))
        }
    }
}

fn require_read_access(status: kuberic_runtime::protocol::types::AccessStatus) -> Result<()> {
    if status == kuberic_runtime::protocol::types::AccessStatus::Granted {
        Ok(())
    } else {
        Err(RuntimeError::ReadClosed(status))
    }
}

#[async_trait]
impl StatefulServiceReplica for KvService {
    async fn open(self: Arc<Self>, context: OpenContext) -> Result<Arc<dyn Replicator>> {
        let mut streams = self.streams.lock().await;
        if *self.stream_stop.borrow() {
            return Err(RuntimeError::Closed);
        }
        if let Some(control) = self.control.read().await.clone() {
            return Ok(control);
        }
        *self.partition.write().await = Some(context.partition.clone());
        let factory: Arc<dyn ReplicatorFactory> = Arc::new(DefaultReplicatorFactory::new(
            self.provider.persistence().clone(),
        ));
        let interfaces = context
            .partition
            .with_factory(factory)
            .create_replicator(
                Some(self.provider.clone()),
                Some(ReplicatorSettings {
                    replication_address: self.replication_address.clone(),
                }),
            )
            .await?;
        let control = interfaces.replicator();
        let state_replicator = interfaces
            .state_replicator()
            .expect("default operation/copy capability");
        let replication = state_replicator.get_replication_stream().await?;
        let copy = state_replicator.get_copy_stream().await?;
        if *self.stream_stop.borrow() {
            return Err(RuntimeError::Closed);
        }
        spawn_stream_consumer(
            &mut streams,
            &self.stream_stop,
            self.provider.persistence().clone(),
            replication,
        );
        spawn_stream_consumer(
            &mut streams,
            &self.stream_stop,
            self.provider.persistence().clone(),
            copy,
        );
        *self.state_replicator.write().await = Some(state_replicator);
        *self.control.write().await = Some(control.clone());
        Ok(control)
    }

    async fn change_role(&self, role: ReplicaRole) -> Result<RoleChange> {
        *self.role.lock().unwrap() = role;
        Ok(RoleChange {
            service_address: (role == ReplicaRole::Primary)
                .then(|| "http://0.0.0.0:8080".to_string()),
        })
    }

    async fn close(&self) -> Result<()> {
        self.stream_stop.send_replace(true);
        let _streams = self.streams.lock().await;
        self.state_replicator.write().await.take();
        self.control.write().await.take();
        self.partition.write().await.take();
        *self.role.lock().unwrap() = ReplicaRole::None;
        Ok(())
    }

    fn abort(&self) {
        self.stream_stop.send_replace(true);
        if let Ok(mut state_replicator) = self.state_replicator.try_write() {
            state_replicator.take();
        }
        if let Ok(mut control) = self.control.try_write() {
            control.take();
        }
        if let Ok(mut partition) = self.partition.try_write() {
            partition.take();
        }
        if let Ok(mut role) = self.role.lock() {
            *role = ReplicaRole::None;
        }
    }
}

fn spawn_stream_consumer(
    streams: &mut StreamConsumers,
    stop: &watch::Sender<bool>,
    persistence: Arc<KvPersistence>,
    stream: OperationStream,
) {
    let mut stop = stop.subscribe();
    streams.tasks.spawn(async move {
        tokio::select! {
            biased;
            _ = stop.wait_for(|stopped| *stopped) => Ok(()),
            result = consume_stream(persistence, stream) => result,
        }
    });
}

async fn consume_stream(
    persistence: Arc<KvPersistence>,
    mut stream: OperationStream,
) -> Result<()> {
    while let Some(operation) = stream.get_operation().await? {
        let result = operation.apply_to(persistence.as_ref()).await;
        match result {
            Ok(progress) => {
                let _ = operation.acknowledge(progress);
            }
            Err(error) => {
                let _ = operation.reject(error);
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    use kuberic_runtime::replicator::stream::OperationMetadata;
    use kuberic_runtime::replicator::stream::OperationSender;

    fn service() -> (tempfile::TempDir, KvService) {
        let directory = tempfile::Builder::new()
            .prefix("kvstore2-stream-test-")
            .tempdir_in(".")
            .unwrap();
        let persistence = Arc::new(KvPersistence::open(directory.path()).unwrap());
        let service = KvService::new(
            persistence,
            "http://127.0.0.1:50052".into(),
            CopyGate::disabled(),
        );
        (directory, service)
    }

    async fn attach_streams(service: &KvService) -> [OperationSender; 2] {
        let mut streams = service.streams.lock().await;
        std::array::from_fn(|_| {
            let (sender, stream) = OperationStream::channel(1);
            spawn_stream_consumer(
                &mut streams,
                &service.stream_stop,
                service.persistence().clone(),
                stream,
            );
            sender
        })
    }

    #[tokio::test]
    async fn close_and_abort_preserve_stream_completion_for_drain() {
        for abort in [false, true] {
            let (_directory, service) = service();
            let senders = attach_streams(&service).await;
            assert_eq!(Arc::strong_count(service.persistence()), 3);
            if abort {
                service.abort();
            } else {
                service.close().await.unwrap();
            }
            assert_eq!(service.streams.lock().await.tasks.len(), 2);
            tokio::time::timeout(Duration::from_secs(5), service.shutdown_and_drain())
                .await
                .expect("consumers must stop even with live stream senders")
                .unwrap();
            assert!(service.streams.lock().await.tasks.is_empty());
            assert_eq!(Arc::strong_count(service.persistence()), 1);
            service.shutdown_and_drain().await.unwrap();
            drop(senders);
        }
    }

    #[tokio::test]
    async fn drain_before_open_is_repeatable() {
        let (_directory, service) = service();
        service.shutdown_and_drain().await.unwrap();
        service.abort();
        service.shutdown_and_drain().await.unwrap();
        assert!(matches!(
            service.get("key").await,
            Err(RuntimeError::NotOpen)
        ));
        assert!(matches!(
            service.replicate_put("key".into(), "value".into()).await,
            Err(RuntimeError::NotOpen)
        ));
    }

    #[tokio::test]
    async fn drain_awaits_all_consumers_and_retains_unexpected_failures() {
        let (_directory, service) = service();
        let finished = Arc::new(AtomicBool::new(false));
        {
            let mut streams = service.streams.lock().await;
            streams.tasks.spawn(async {
                Err(RuntimeError::Application(
                    "unexpected consumer error".into(),
                ))
            });
            streams.tasks.spawn(async {
                panic!("unexpected consumer panic");
            });
            let finished = finished.clone();
            streams.tasks.spawn(async move {
                finished.store(true, Ordering::Release);
                Ok(())
            });
        }
        let error = service.shutdown_and_drain().await.unwrap_err().to_string();
        assert!(error.contains("unexpected consumer error"));
        assert!(error.contains("unexpected consumer panic"));
        assert!(finished.load(Ordering::Acquire));
        assert!(service.streams.lock().await.tasks.is_empty());
        assert_eq!(
            service.shutdown_and_drain().await.unwrap_err().to_string(),
            error
        );
    }

    #[tokio::test]
    async fn cancelled_drain_keeps_join_ownership_and_errors() {
        let (_directory, service) = service();
        let (started, started_rx) = tokio::sync::oneshot::channel();
        let (release, release_rx) = tokio::sync::oneshot::channel();
        {
            let mut streams = service.streams.lock().await;
            streams.tasks.spawn(async {
                Err(RuntimeError::Application(
                    "retained consumer failure".into(),
                ))
            });
            streams.tasks.spawn(async move {
                started.send(()).unwrap();
                release_rx.await.unwrap();
                Ok(())
            });
        }
        {
            let drain = service.shutdown_and_drain();
            tokio::pin!(drain);
            tokio::select! {
                result = &mut drain => panic!("drain completed before task release: {result:?}"),
                result = started_rx => result.unwrap(),
            }
        }
        assert!(!service.streams.lock().await.tasks.is_empty());
        release.send(()).unwrap();
        let error = service.shutdown_and_drain().await.unwrap_err().to_string();
        assert!(error.contains("retained consumer failure"));
        assert!(service.streams.lock().await.tasks.is_empty());
    }

    #[tokio::test]
    async fn stream_replication_commits_without_reapplying_duplicate_payload() {
        use futures::TryStreamExt;
        use kuberic_runtime::engine::DurableState;
        let (_directory, service) = service();
        let senders = attach_streams(&service).await;
        let payload = KvPersistence::encode_put("key".into(), "value".into()).unwrap();
        let send = |committed_lsn, data| {
            senders[0].send(
                OperationMetadata::Replication {
                    lsn: 1,
                    committed_lsn,
                },
                data,
            )
        };
        assert_eq!(send(0, payload.clone()).await.unwrap().committed_lsn, 0);
        service.persistence().fail_next_persist_for_test();
        assert!(send(1, payload.clone()).await.is_err());
        assert_eq!(
            service
                .persistence()
                .durable_progress()
                .await
                .unwrap()
                .committed_lsn,
            0
        );
        assert_eq!(send(1, payload.clone()).await.unwrap().committed_lsn, 1);
        assert_eq!(send(1, payload).await.unwrap().committed_lsn, 1);
        assert!(
            send(
                1,
                KvPersistence::encode_put("key".into(), "corrupt".into()).unwrap()
            )
            .await
            .is_err()
        );
        let retained: Vec<_> = service
            .persistence()
            .get_replication_operations(1, 1)
            .await
            .unwrap()
            .try_collect()
            .await
            .unwrap();
        assert_eq!(retained.len(), 1);
        assert_eq!(service.persistence().stream_call_counts(), (1, 2));
        assert_eq!(
            retained[0].committed_lsn, 0,
            "immutable applied payload was rewritten"
        );
        assert_eq!(service.persistence().get("key").as_deref(), Some("value"));
        service.shutdown_and_drain().await.unwrap();
    }

    #[tokio::test]
    async fn provider_rejection_does_not_terminate_stream_consumers() {
        let (_directory, service) = service();
        let senders = attach_streams(&service).await;
        let metadata = OperationMetadata::Replication {
            lsn: 1,
            committed_lsn: 0,
        };
        assert!(matches!(
            senders[0]
                .send(metadata.clone(), bytes::Bytes::from_static(b"invalid"))
                .await,
            Err(RuntimeError::Application(_))
        ));
        let progress = senders[0]
            .send(
                metadata,
                KvPersistence::encode_put("key".into(), "value".into()).unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(progress.applied_lsn, 1);
        assert_eq!(service.persistence().get("key").as_deref(), Some("value"));
        service.shutdown_and_drain().await.unwrap();
    }
}
