//! Production SQLite service over native V1 assignment and service-owned WAL
//! commitment/publication. No engine can open or commit the application store.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use async_trait::async_trait;
use futures::{StreamExt, stream};
use kuberic_runtime::application::{OpenContext, OperationDataStream, RoleChange};
use kuberic_runtime::protocol::types::{AccessStatus, Epoch, FaultType, ReplicaRole};
use kuberic_runtime::replicator::stream::OperationStream;
use kuberic_runtime::replicator::{
    DefaultReplicatorFactory, Replicator, ReplicatorSettings, StateReplicator,
    StatefulServicePartition,
};
use kuberic_runtime::{Result, RuntimeError, StateProvider, StatefulServiceReplica};
use tokio::sync::{Mutex as AsyncMutex, watch};
use tokio::task::JoinSet;

use crate::SqlitePersistence;
use crate::connection::Statement;
use crate::state::persistence_error;
use crate::v1::{Assignment, Committer, Execution};

#[derive(Clone, Copy, PartialEq, Eq)]
enum CopyCompletion {
    Pending,
    Complete,
    Retired,
}

pub struct SqliteProvider {
    committer: Arc<OnceLock<Arc<Committer>>>,
    #[cfg(any(test, feature = "testing"))]
    copy_snapshot: crate::testing::PauseGate,
}

impl SqliteProvider {
    fn committer(&self) -> Result<&Arc<Committer>> {
        self.committer.get().ok_or(RuntimeError::NotOpen)
    }
}

#[async_trait]
impl StateProvider for SqliteProvider {
    async fn update_epoch(&self, _epoch: Epoch, previous_epoch_last_lsn: i64) -> Result<()> {
        if previous_epoch_last_lsn < 0 {
            return Err(RuntimeError::Application(
                "negative SQLite epoch boundary".into(),
            ));
        }
        // SQLite has no epoch-indexed application state to mutate. Native owner
        // barriers, not an independent service oracle, sequence this callback.
        self.committer()?.progress().map_err(persistence_error)?;
        Ok(())
    }
    async fn last_committed_lsn(&self) -> Result<i64> {
        self.committer()?.progress().map_err(persistence_error)
    }
    async fn get_copy_context(&self) -> Result<OperationDataStream> {
        Ok(Box::pin(stream::empty()))
    }
    async fn get_copy_state(
        &self,
        up_to_lsn: i64,
        mut context: OperationDataStream,
    ) -> Result<OperationDataStream> {
        if let Some(item) = context.next().await {
            item?;
            return Err(RuntimeError::Application(
                "SQLite does not use copy context".into(),
            ));
        }
        let data = self
            .committer()?
            .snapshot(up_to_lsn)
            .await
            .map_err(persistence_error)?;
        #[cfg(any(test, feature = "testing"))]
        self.copy_snapshot.pause().await;
        Ok(Box::pin(stream::iter([Ok(data)])))
    }
    async fn on_data_loss(&self) -> Result<bool> {
        Ok(false)
    }
}

#[derive(Default)]
struct Consumers {
    tasks: JoinSet<Result<()>>,
    errors: Vec<String>,
}

pub struct SqliteService {
    persistence: Arc<SqlitePersistence>,
    provider: Arc<SqliteProvider>,
    partition: Arc<Mutex<Option<StatefulServicePartition>>>,
    control: Mutex<Option<Arc<dyn Replicator>>>,
    replicator: Mutex<Option<Arc<dyn StateReplicator>>>,
    committer: Arc<OnceLock<Arc<Committer>>>,
    active: Arc<AtomicBool>,
    role: Mutex<ReplicaRole>,
    replication_address: String,
    stop: watch::Sender<bool>,
    consumers: AsyncMutex<Consumers>,
    #[cfg(feature = "testing")]
    stream_exit: Arc<crate::testing::PauseGate>,
    #[cfg(feature = "testing")]
    fail_stream_exit: Arc<AtomicBool>,
}

impl SqliteService {
    pub fn new(
        persistence: Arc<SqlitePersistence>,
        replication_address: String,
    ) -> std::io::Result<Self> {
        Self::with_store(persistence, replication_address)
    }
    pub fn deferred(root: PathBuf, replication_address: String) -> std::io::Result<Self> {
        Self::with_store(
            Arc::new(SqlitePersistence::deferred(root)?),
            replication_address,
        )
    }
    fn with_store(
        persistence: Arc<SqlitePersistence>,
        replication_address: String,
    ) -> std::io::Result<Self> {
        let committer = Arc::new(OnceLock::new());
        Ok(Self {
            persistence,
            provider: Arc::new(SqliteProvider {
                committer: committer.clone(),
                #[cfg(any(test, feature = "testing"))]
                copy_snapshot: Default::default(),
            }),
            partition: Arc::new(Mutex::new(None)),
            control: Mutex::new(None),
            replicator: Mutex::new(None),
            committer,
            active: Arc::new(AtomicBool::new(false)),
            role: Mutex::new(ReplicaRole::None),
            replication_address,
            stop: watch::channel(false).0,
            consumers: AsyncMutex::new(Consumers::default()),
            #[cfg(feature = "testing")]
            stream_exit: Default::default(),
            #[cfg(feature = "testing")]
            fail_stream_exit: Default::default(),
        })
    }
    pub fn persistence(&self) -> &Arc<SqlitePersistence> {
        &self.persistence
    }
    pub fn provider(&self) -> &Arc<SqliteProvider> {
        &self.provider
    }
    pub fn partition(&self) -> Result<StatefulServicePartition> {
        self.partition
            .lock()
            .unwrap()
            .clone()
            .ok_or(RuntimeError::NotOpen)
    }
    fn committer(&self) -> Result<&Arc<Committer>> {
        self.committer.get().ok_or(RuntimeError::NotOpen)
    }

    pub(crate) async fn execute(&self, statements: Vec<Statement>) -> Result<Execution> {
        self.check_access(true).await?;
        let mut write = self
            .committer()?
            .begin_statements(statements)
            .map_err(persistence_error)?;
        match write.wait_execution().await {
            Ok(execution) => Ok(execution),
            Err(error) => Err(match write.admitted_lsn() {
                Some(lsn) => RuntimeError::AdmittedOperationFailed {
                    lsn,
                    reason: error.to_string(),
                },
                None => persistence_error(error),
            }),
        }
    }

    pub(crate) async fn query(
        &self,
        sql: String,
        params: Vec<rusqlite::types::Value>,
    ) -> Result<(Vec<String>, Vec<Vec<rusqlite::types::Value>>)> {
        self.check_access(false).await?;
        let committer = self.committer()?.clone();
        tokio::task::spawn_blocking(move || committer.query_with_params(&sql, &params))
            .await
            .map_err(|error| RuntimeError::Application(error.to_string()))?
            .map_err(persistence_error)
    }

    async fn check_access(&self, write: bool) -> Result<()> {
        if !self.active.load(Ordering::Acquire) {
            return Err(RuntimeError::NotPrimary);
        }
        let partition = self.partition()?;
        let status = if write {
            partition.get_write_status().await?
        } else {
            partition.get_read_status().await?
        };
        if status != AccessStatus::Granted {
            return Err(if write {
                RuntimeError::WriteClosed(status)
            } else {
                RuntimeError::ReadClosed(status)
            });
        }
        Ok(())
    }

    #[cfg(any(test, feature = "testing"))]
    pub fn copy_snapshot_gate(&self) -> &crate::testing::PauseGate {
        &self.provider.copy_snapshot
    }
    #[cfg(any(test, feature = "testing"))]
    pub fn primary_application_active_for_test(&self) -> bool {
        self.active.load(Ordering::Acquire)
    }
    #[cfg(feature = "testing")]
    pub fn committer_for_test(&self) -> Result<Arc<crate::v1_test_bridge::Committer>> {
        self.committer().cloned()
    }

    #[cfg(feature = "testing")]
    pub fn consumer_exit_gate(&self) -> &crate::testing::PauseGate {
        &self.stream_exit
    }

    #[cfg(feature = "testing")]
    pub fn fail_next_consumer_exit(&self) {
        self.fail_stream_exit.store(true, Ordering::Release);
    }

    fn spawn_consumer(
        &self,
        consumers: &mut Consumers,
        task: impl std::future::Future<Output = Result<()>> + Send + 'static,
    ) {
        #[cfg(feature = "testing")]
        let (exit, fail) = (self.stream_exit.clone(), self.fail_stream_exit.clone());
        consumers.tasks.spawn(async move {
            let result = task.await;
            #[cfg(feature = "testing")]
            {
                exit.pause().await;
                if fail.swap(false, Ordering::AcqRel) {
                    return Err(RuntimeError::Application(
                        "injected SQLite consumer join failure".into(),
                    ));
                }
            }
            result
        });
    }

    async fn start_secondary(&self, copy: bool) -> Result<()> {
        let replicator = self
            .replicator
            .lock()
            .unwrap()
            .clone()
            .ok_or(RuntimeError::NotOpen)?;
        let store = self.committer()?.clone();
        let partition = self.partition()?;
        let mut consumers = self.consumers.lock().await;
        while let Some(result) = consumers.tasks.try_join_next() {
            match result {
                Ok(Ok(())) => {}
                Ok(Err(error)) => consumers.errors.push(error.to_string()),
                Err(error) => consumers.errors.push(error.to_string()),
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
            store.begin_copy().map_err(persistence_error)?;
            let mut stream = replicator.copy_operations()?;
            let store = store.clone();
            let partition = partition.clone();
            let mut stop = self.stop.subscribe();
            self.spawn_consumer(&mut consumers, async move {
                let receive = async move {
                    let mut completed = false;
                    while let Some(operation) = match stream.get_operation().await {
                        Ok(operation) => operation,
                        Err(RuntimeError::Closed) => {
                            tracing::debug!("SQLite copy receiver retired");
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
                        if operation.end_of_stream {
                            if let Err(error) = store.finish_copy() {
                                return failed(stream, partition, error.to_string()).await;
                            }
                            completed = true;
                            copied.send_replace(CopyCompletion::Complete);
                        }
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
                    Ok(())
                };
                tokio::select! {
                    biased;
                    _ = async { let _ = stop.wait_for(|stopped| *stopped).await; } => Ok(()),
                    result = receive => result,
                }
            });
        }
        let mut stream = replicator.replication_operations()?;
        let mut stop = self.stop.subscribe();
        self.spawn_consumer(&mut consumers, async move {
            let receive = async move {
                let mut completion = completion;
                let mut copied = *completion.borrow();
                if copied == CopyCompletion::Pending {
                    let status = completion
                        .wait_for(|state| *state != CopyCompletion::Pending)
                        .await
                        .map(|state| *state);
                    match status {
                        Ok(state) => copied = state,
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
                if copied == CopyCompletion::Retired {
                    tracing::debug!("SQLite replication observer retired with its copy receiver");
                    return Ok(());
                }
                while let Some(operation) = match stream.get_operation().await {
                    Ok(operation) => operation,
                    Err(RuntimeError::Closed) => {
                        tracing::debug!("SQLite replication receiver retired");
                        return Ok(());
                    }
                    Err(error) => return Err(error),
                } {
                    if !operation.end_of_stream
                        && let Err(error) =
                            store.apply_secondary(operation.lsn, operation.data.clone())
                    {
                        return failed(stream, partition, error.to_string()).await;
                    }
                    operation.acknowledge()?;
                }
                Ok(())
            };
            tokio::select! {
                biased;
                _ = async { let _ = stop.wait_for(|stopped| *stopped).await; } => Ok(()),
                result = receive => result,
            }
        });
        Ok(())
    }

    pub async fn shutdown_and_drain(&self) -> Result<()> {
        self.close().await
    }

    async fn drain_consumers(&self, service: Result<()>) -> Result<()> {
        let mut consumers = self.consumers.lock().await;
        while let Some(result) = consumers.tasks.join_next().await {
            match result {
                Ok(Ok(())) => {}
                Ok(Err(error)) => consumers.errors.push(error.to_string()),
                Err(error) => consumers.errors.push(error.to_string()),
            }
        }
        if consumers.errors.is_empty() {
            service
        } else {
            let errors = consumers.errors.join("; ");
            Err(RuntimeError::Application(match service {
                Ok(()) => errors,
                Err(error) => format!("{error}; {errors}"),
            }))
        }
    }
}

#[async_trait]
impl StatefulServiceReplica for SqliteService {
    async fn open(self: Arc<Self>, context: OpenContext) -> Result<Arc<dyn Replicator>> {
        if *self.stop.borrow() {
            return Err(RuntimeError::Closed);
        }
        if let Some(control) = self.control.lock().unwrap().clone() {
            return Ok(control);
        }
        self.persistence
            .authorize_open(context.mode)
            .map_err(persistence_error)?;
        *self.partition.lock().unwrap() = Some(context.partition.clone());
        let partitions = self.partition.clone();
        let active = self.active.clone();
        let (fault, mut faults) = tokio::sync::mpsc::unbounded_channel::<String>();
        let committer = Arc::new(
            Committer::with_store(
                self.persistence.clone(),
                Arc::new(move |write| {
                    if !active.load(Ordering::Acquire) {
                        return Err(std::io::Error::other("SQLite service is not primary"));
                    }
                    let partition = partitions
                        .lock()
                        .unwrap()
                        .clone()
                        .ok_or_else(|| std::io::Error::other("SQLite service is not open"))?;
                    let status = if write {
                        partition.get_write_status_now()
                    } else {
                        partition.get_read_status_now()
                    };
                    if status != AccessStatus::Granted {
                        return Err(std::io::Error::other(format!(
                            "SQLite host access {status:?}"
                        )));
                    }
                    Ok(())
                }),
                Arc::new(move |reason| {
                    let _ = fault.send(reason);
                }),
            )
            .map_err(persistence_error)?,
        );
        self.committer
            .set(committer)
            .map_err(|_| RuntimeError::Application("SQLite committer already bound".into()))?;
        let partition = context.partition.clone();
        let mut stop = self.stop.subscribe();
        self.consumers.lock().await.tasks.spawn(async move {
            tokio::select! {
                biased;
                _ = async { let _ = stop.wait_for(|stopped| *stopped).await; } => Ok(()),
                reason = faults.recv() => {
                    if let Some(reason) = reason {
                        tracing::error!(%reason, "SQLite service commitment/publication fault");
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
        *self.replicator.lock().unwrap() = interfaces.state_replicator();
        *self.control.lock().unwrap() = Some(control.clone());
        Ok(control)
    }

    async fn change_role(&self, role: ReplicaRole) -> Result<RoleChange> {
        self.active.store(false, Ordering::Release);
        let previous = *self.role.lock().unwrap();
        if role == ReplicaRole::Primary || previous == ReplicaRole::Primary {
            self.committer()?
                .wait_for_role_drain()
                .await
                .map_err(persistence_error)?;
        }
        match role {
            ReplicaRole::Primary => {
                let replicator = self
                    .replicator
                    .lock()
                    .unwrap()
                    .clone()
                    .ok_or(RuntimeError::NotOpen)?;
                self.committer()?
                    .promote(Arc::new(move |data| {
                        let admitted = replicator
                            .begin_replicate(data)
                            .map_err(std::io::Error::other)?;
                        Ok(Assignment {
                            lsn: admitted.lsn,
                            completion: Box::pin(async move {
                                admitted.completion.await.map_err(std::io::Error::other)
                            }),
                        })
                    }))
                    .map_err(persistence_error)?;
                self.active.store(true, Ordering::Release);
            }
            ReplicaRole::IdleSecondary if previous != ReplicaRole::IdleSecondary => {
                self.start_secondary(true).await?
            }
            ReplicaRole::ActiveSecondary if previous == ReplicaRole::Primary => {
                self.committer()?.demote().map_err(persistence_error)?;
                self.start_secondary(false).await?;
            }
            ReplicaRole::ActiveSecondary if previous == ReplicaRole::None => {
                self.start_secondary(true).await?
            }
            ReplicaRole::None if previous == ReplicaRole::Primary => {
                self.committer()?.demote().map_err(persistence_error)?
            }
            ReplicaRole::IdleSecondary | ReplicaRole::ActiveSecondary | ReplicaRole::None => {}
        }
        *self.role.lock().unwrap() = role;
        Ok(RoleChange {
            service_address: None,
        })
    }
    async fn close(&self) -> Result<()> {
        self.active.store(false, Ordering::Release);
        let result = if let Some(committer) = self.committer.get() {
            match committer.close().await {
                Err(error) if crate::v1::is_abort(&error) && *self.stop.borrow() => Ok(()),
                result => result.map_err(persistence_error),
            }
        } else {
            Ok(())
        };
        self.stop.send_replace(true);
        self.control.lock().unwrap().take();
        self.replicator.lock().unwrap().take();
        self.partition.lock().unwrap().take();
        self.drain_consumers(result).await
    }
    fn abort(&self) {
        self.active.store(false, Ordering::Release);
        if let Some(committer) = self.committer.get() {
            committer.abort();
        }
        self.stop.send_replace(true);
        if let Ok(mut partition) = self.partition.try_lock() {
            partition.take();
        }
        if let Ok(mut replicator) = self.replicator.try_lock() {
            replicator.take();
        }
        if let Ok(mut control) = self.control.try_lock() {
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
