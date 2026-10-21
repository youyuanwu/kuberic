//! Service-owned commitment over the production classic V1 replicator.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use kuberic_runtime::application::{OpenContext, RoleChange, StatefulServiceReplica};
use kuberic_runtime::protocol::types::{AccessStatus, FaultType, ReplicaRole};
use kuberic_runtime::replicator::stream::OperationStream;
use kuberic_runtime::replicator::{
    DefaultReplicatorFactory, Replicator, ReplicatorSettings, StateReplicator,
    StatefulServicePartition,
};
use kuberic_runtime::{Result, RuntimeError};
use tokio::sync::{Mutex as AsyncMutex, RwLock, watch};
use tokio::task::JoinSet;

use crate::persistence::KvPersistence;
use crate::state::{CopyGate, KvStateProvider};
use crate::v1::{Assignment, Committer};

#[derive(Clone, Copy, PartialEq, Eq)]
enum CopyCompletion {
    Pending,
    Complete,
    Retired,
}

pub struct KvService {
    provider: Arc<KvStateProvider>,
    replication_address: String,
    state_replicator: RwLock<Option<Arc<dyn StateReplicator>>>,
    control: RwLock<Option<Arc<dyn Replicator>>>,
    partition: RwLock<Option<StatefulServicePartition>>,
    committer: Mutex<Option<Arc<Committer>>>,
    role: Mutex<ReplicaRole>,
    stop: watch::Sender<bool>,
    streams: AsyncMutex<Consumers>,
    #[cfg(feature = "testing")]
    stream_exit: Arc<Mutex<Option<Arc<StreamExitGate>>>>,
}

#[cfg(feature = "testing")]
pub struct StreamExitGate {
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

#[cfg(feature = "testing")]
impl StreamExitGate {
    pub async fn entered(&self) {
        self.entered.notified().await;
    }
    pub fn release(&self) {
        self.release.notify_one();
    }
}

#[derive(Default)]
struct Consumers {
    tasks: JoinSet<Result<()>>,
    errors: Vec<String>,
}

fn application(error: std::io::Error) -> RuntimeError {
    if crate::v1::is_abort(&error) {
        RuntimeError::OperationCancelled
    } else {
        RuntimeError::Application(error.to_string())
    }
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
            committer: Mutex::new(None),
            role: Mutex::new(ReplicaRole::None),
            stop: watch::channel(false).0,
            streams: AsyncMutex::new(Consumers::default()),
            #[cfg(feature = "testing")]
            stream_exit: Arc::new(Mutex::new(None)),
        }
    }

    pub fn persistence(&self) -> &Arc<KvPersistence> {
        self.provider.persistence()
    }

    #[cfg(feature = "testing")]
    pub fn pause_next_commit(&self) -> Result<crate::v1_test_bridge::CommitGate> {
        Ok(self.committer()?.pause_next_commit())
    }

    #[cfg(feature = "testing")]
    pub fn pause_next_stream_exit(&self) -> Arc<StreamExitGate> {
        let gate = Arc::new(StreamExitGate {
            entered: tokio::sync::Notify::new(),
            release: tokio::sync::Notify::new(),
        });
        *self.stream_exit.lock().unwrap() = Some(gate.clone());
        gate
    }

    #[cfg(feature = "testing")]
    pub async fn retained_consumer_count(&self) -> usize {
        self.streams.lock().await.tasks.len()
    }

    fn spawn_stream(
        &self,
        consumers: &mut Consumers,
        task: impl std::future::Future<Output = Result<()>> + Send + 'static,
    ) {
        #[cfg(feature = "testing")]
        let exit = self.stream_exit.clone();
        consumers.tasks.spawn(async move {
            let result = task.await;
            #[cfg(feature = "testing")]
            {
                let gate = exit.lock().unwrap().take();
                if let Some(gate) = gate {
                    gate.entered.notify_one();
                    gate.release.notified().await;
                }
            }
            result
        });
    }

    fn committer(&self) -> Result<Arc<Committer>> {
        self.committer
            .lock()
            .unwrap()
            .clone()
            .ok_or(RuntimeError::NotOpen)
    }

    pub async fn replicate_put(&self, key: String, value: String) -> Result<i64> {
        let replicator = self
            .state_replicator
            .read()
            .await
            .clone()
            .ok_or(RuntimeError::NotOpen)?;
        let mut assigned = None;
        let write = self.committer()?.begin_put(key, value, |data| {
            let admitted = replicator
                .begin_replicate(data)
                .map_err(std::io::Error::other)?;
            assigned = Some(admitted.lsn);
            Ok(Assignment {
                lsn: admitted.lsn,
                completion: Box::pin(async move {
                    admitted.completion.await.map_err(std::io::Error::other)
                }),
            })
        });
        let mut write = write.map_err(|error| match assigned {
            Some(lsn) => RuntimeError::AdmittedOperationFailed {
                lsn,
                reason: error.to_string(),
            },
            None => application(error),
        })?;
        write
            .wait()
            .await
            .map_err(|error| RuntimeError::AdmittedOperationFailed {
                lsn: write.lsn,
                reason: error.to_string(),
            })
    }

    pub async fn get(&self, key: &str) -> Result<Option<String>> {
        let partition = self
            .partition
            .read()
            .await
            .clone()
            .ok_or(RuntimeError::NotOpen)?;
        let status = partition.get_read_status().await?;
        if status != AccessStatus::Granted {
            return Err(RuntimeError::ReadClosed(status));
        }
        self.committer()?.get(key, || Ok(())).map_err(application)
    }

    pub async fn shutdown_and_drain(&self) -> Result<()> {
        self.close().await
    }

    async fn drain_consumers(&self, service: Result<()>) -> Result<()> {
        let mut consumers = self.streams.lock().await;
        while let Some(result) = consumers.tasks.join_next().await {
            match result {
                Ok(Ok(())) => {}
                Ok(Err(error)) => consumers.errors.push(error.to_string()),
                Err(error) => consumers
                    .errors
                    .push(format!("KV consumer failed: {error}")),
            }
        }
        if consumers.errors.is_empty() {
            service
        } else {
            let consumers = consumers.errors.join("; ");
            Err(RuntimeError::Application(match service {
                Ok(()) => consumers,
                Err(error) => format!("{error}; {consumers}"),
            }))
        }
    }

    async fn start_secondary(&self, copy: bool) -> Result<()> {
        let replicator = self
            .state_replicator
            .read()
            .await
            .clone()
            .ok_or(RuntimeError::NotOpen)?;
        let committer = self.committer()?;
        let partition = self
            .partition
            .read()
            .await
            .clone()
            .ok_or(RuntimeError::NotOpen)?;
        let mut consumers = self.streams.lock().await;
        if *self.stop.borrow() {
            return Err(RuntimeError::Closed);
        }
        while let Some(result) = consumers.tasks.try_join_next() {
            match result {
                Ok(Ok(())) => {}
                Ok(Err(error)) => consumers.errors.push(error.to_string()),
                Err(error) => consumers
                    .errors
                    .push(format!("KV consumer failed: {error}")),
            }
        }
        if !consumers.errors.is_empty() {
            return Err(RuntimeError::Application(consumers.errors.join("; ")));
        }
        let (copied, completion) = watch::channel(if copy {
            CopyCompletion::Pending
        } else {
            CopyCompletion::Complete
        });
        if copy {
            committer.begin_copy().map_err(application)?;
            let stream = replicator.copy_operations()?;
            let store = committer.clone();
            let partition = partition.clone();
            let mut stop = self.stop.subscribe();
            self.spawn_stream(&mut consumers, async move {
                tokio::select! {
                    biased;
                    _ = async { let _ = stop.wait_for(|stopped| *stopped).await; } => Ok(()),
                    result = consume_copy(store, stream, copied, partition) => result,
                }
            });
        }
        let stream = replicator.replication_operations()?;
        let mut stop = self.stop.subscribe();
        self.spawn_stream(&mut consumers, async move {
            tokio::select! {
                biased;
                _ = async { let _ = stop.wait_for(|stopped| *stopped).await; } => Ok(()),
                result = consume_replication(committer, stream, completion, partition) => result,
            }
        });
        Ok(())
    }
}

#[async_trait]
impl StatefulServiceReplica for KvService {
    async fn open(self: Arc<Self>, context: OpenContext) -> Result<Arc<dyn Replicator>> {
        if *self.stop.borrow() {
            return Err(RuntimeError::Closed);
        }
        if let Some(control) = self.control.read().await.clone() {
            return Ok(control);
        }
        self.persistence().authorize_open(context.mode)?;
        *self.partition.write().await = Some(context.partition.clone());
        let (fault, mut faults) = tokio::sync::mpsc::unbounded_channel::<String>();
        let committer = Arc::new(
            Committer::with_store(
                self.persistence().clone(),
                Arc::new(move |reason| {
                    let _ = fault.send(reason);
                }),
            )
            .map_err(application)?,
        );
        self.provider.bind_committer(committer.clone())?;
        *self.committer.lock().unwrap() = Some(committer);
        let partition = context.partition.clone();
        let mut stop = self.stop.subscribe();
        self.streams.lock().await.tasks.spawn(async move {
            tokio::select! {
                biased;
                _ = async { let _ = stop.wait_for(|stopped| *stopped).await; } => Ok(()),
                reason = faults.recv() => {
                    if let Some(reason) = reason {
                        tracing::error!(%reason, "KV service commitment fault");
                        partition.report_fault(FaultType::Transient).await?;
                    }
                    Ok(())
                }
            }
        });
        let interfaces = context
            .partition
            .with_factory(Arc::new(DefaultReplicatorFactory::new()))
            .create_replicator(
                Some(self.provider.clone()),
                Some(ReplicatorSettings {
                    replication_address: self.replication_address.clone(),
                }),
            )
            .await?;
        let control = interfaces.replicator();
        if *self.stop.borrow() {
            control.abort();
            return Err(RuntimeError::Closed);
        }
        *self.state_replicator.write().await = interfaces.state_replicator();
        *self.control.write().await = Some(control.clone());
        Ok(control)
    }

    async fn change_role(&self, role: ReplicaRole) -> Result<RoleChange> {
        if *self.stop.borrow() {
            return Err(RuntimeError::Closed);
        }
        let previous = *self.role.lock().unwrap();
        let committer = self.committer()?;
        if role == ReplicaRole::Primary || previous == ReplicaRole::Primary {
            committer.wait_for_role_drain().await.map_err(application)?;
        }
        match role {
            ReplicaRole::Primary => committer.promote().map_err(application)?,
            ReplicaRole::IdleSecondary => self.start_secondary(true).await?,
            ReplicaRole::ActiveSecondary if previous == ReplicaRole::Primary => {
                committer.demote().map_err(application)?;
                self.start_secondary(false).await?;
            }
            ReplicaRole::ActiveSecondary if previous == ReplicaRole::None => {
                self.start_secondary(true).await?
            }
            ReplicaRole::None if previous == ReplicaRole::Primary => {
                committer.demote().map_err(application)?
            }
            ReplicaRole::ActiveSecondary | ReplicaRole::None => {}
        }
        *self.role.lock().unwrap() = role;
        Ok(RoleChange {
            service_address: (role == ReplicaRole::Primary).then(|| "http://0.0.0.0:8080".into()),
        })
    }

    async fn close(&self) -> Result<()> {
        let committer = self.committer.lock().unwrap().clone();
        let result = if let Some(committer) = committer {
            committer.close().await.map_err(application)
        } else {
            Ok(())
        };
        self.stop.send_replace(true);
        self.state_replicator.write().await.take();
        self.control.write().await.take();
        self.partition.write().await.take();
        *self.role.lock().unwrap() = ReplicaRole::None;
        self.drain_consumers(result).await
    }

    fn abort(&self) {
        if let Some(committer) = self.committer.lock().unwrap().as_ref() {
            committer.abort();
        }
        self.stop.send_replace(true);
        if let Ok(mut partition) = self.partition.try_write() {
            partition.take();
        }
        if let Ok(mut replicator) = self.state_replicator.try_write() {
            replicator.take();
        }
        if let Ok(mut control) = self.control.try_write() {
            control.take();
        }
    }
}

async fn failed(
    stream: OperationStream,
    partition: StatefulServicePartition,
    reason: String,
) -> Result<()> {
    let native = stream.report_fault(reason.clone());
    let host = partition.report_fault(FaultType::Transient).await;
    native?;
    host?;
    Err(RuntimeError::Application(reason))
}

async fn consume_copy(
    store: Arc<Committer>,
    mut stream: OperationStream,
    copied: watch::Sender<CopyCompletion>,
    partition: StatefulServicePartition,
) -> Result<()> {
    let mut completed = false;
    while let Some(operation) = match stream.get_operation().await {
        Ok(operation) => operation,
        Err(RuntimeError::Closed) => {
            tracing::debug!("KV copy receiver retired");
            copied.send_replace(CopyCompletion::Retired);
            return Ok(());
        }
        Err(error) => return Err(error),
    } {
        if !operation.end_of_stream
            && let Err(error) = store.install_copy(operation.data.clone())
        {
            return failed(stream, partition, error.to_string()).await;
        }
        completed |= operation.end_of_stream;
        operation.acknowledge()?;
    }
    if !completed {
        return failed(
            stream,
            partition,
            "copy receiver ended before native EOF".into(),
        )
        .await;
    }
    if let Err(error) = store.finish_copy() {
        return failed(stream, partition, error.to_string()).await;
    }
    copied.send_replace(CopyCompletion::Complete);
    Ok(())
}

async fn consume_replication(
    store: Arc<Committer>,
    mut stream: OperationStream,
    mut copied: watch::Receiver<CopyCompletion>,
    partition: StatefulServicePartition,
) -> Result<()> {
    let mut completion = *copied.borrow();
    if completion == CopyCompletion::Pending {
        let status = copied
            .wait_for(|state| *state != CopyCompletion::Pending)
            .await
            .map(|state| *state);
        match status {
            Ok(state) => completion = state,
            Err(_) => {
                return failed(
                    stream,
                    partition,
                    "copy receiver stopped before durable completion".into(),
                )
                .await;
            }
        }
    }
    if completion == CopyCompletion::Retired {
        tracing::debug!("KV replication observer retired with its copy receiver");
        return Ok(());
    }
    while let Some(operation) = match stream.get_operation().await {
        Ok(operation) => operation,
        Err(RuntimeError::Closed) => {
            tracing::debug!("KV replication receiver retired");
            return Ok(());
        }
        Err(error) => return Err(error),
    } {
        if !operation.end_of_stream
            && let Err(error) = store.apply_secondary(operation.lsn, operation.data.clone())
        {
            return failed(stream, partition, error.to_string()).await;
        }
        operation.acknowledge()?;
    }
    Ok(())
}

#[cfg(test)]
mod close_tests {
    use super::*;

    #[tokio::test]
    async fn trait_close_joins_consumers_and_retains_failure_on_retry() {
        let directory = tempfile::tempdir().unwrap();
        let service = KvService::new(
            Arc::new(KvPersistence::open(directory.path()).unwrap()),
            "http://127.0.0.1:0".into(),
            CopyGate::disabled(),
        );
        service
            .streams
            .lock()
            .await
            .tasks
            .spawn(async { Err(RuntimeError::Application("owned consumer failure".into())) });
        let first = service.close().await.unwrap_err().to_string();
        assert!(first.contains("owned consumer failure"));
        assert!(service.streams.lock().await.tasks.is_empty());
        assert_eq!(service.close().await.unwrap_err().to_string(), first);
        assert_eq!(
            service.shutdown_and_drain().await.unwrap_err().to_string(),
            first
        );
    }
}
