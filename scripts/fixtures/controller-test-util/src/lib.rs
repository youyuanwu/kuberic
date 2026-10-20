//! Independent downstream application: no example or checkout-private dependency.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use bytes::Bytes;
use futures::{StreamExt, stream};
use kuberic_runtime::application::{
    CopyChunk, DurableApplicationProgress, OpenContext, OpenMode, Operation, OperationDataStream,
    RoleChange,
};
use kuberic_runtime::engine::{DurableState, RetainedOperationStream};
use kuberic_runtime::protocol::types::{AccessStatus, Epoch, FaultType, OperationId, ReplicaRole};
use kuberic_runtime::replicator::stream::OperationStream;
use kuberic_runtime::replicator::{
    DefaultReplicatorFactory, Replicator, ReplicatorSettings, StateReplicator,
    StatefulServicePartition,
};
use kuberic_runtime::{Result, RuntimeError, StateProvider, StatefulServiceReplica};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::{Mutex as AsyncMutex, Notify, RwLock, oneshot, watch};
use tokio::task::JoinSet;

fn app_error(error: impl std::fmt::Display) -> RuntimeError {
    RuntimeError::Application(error.to_string())
}

fn trace_counter() -> bool {
    cfg!(test) || std::env::var_os("KUBERIC_COUNTER_TRACE").is_some()
}

fn decode(bytes: &[u8]) -> Result<i64> {
    let value: [u8; 8] = bytes
        .try_into()
        .map_err(|_| app_error("counter operation must contain exactly eight bytes"))?;
    Ok(i64::from_le_bytes(value))
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    value: i64,
    committed: i64,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CounterState {
    format: u32,
    epoch: Epoch,
    base_lsn: i64,
    base_value: i64,
    applied: i64,
    committed: i64,
    operations: BTreeMap<i64, Record>,
}

impl Default for CounterState {
    fn default() -> Self {
        Self {
            format: 1,
            epoch: Epoch::default(),
            base_lsn: 0,
            base_value: 0,
            applied: 0,
            committed: 0,
            operations: BTreeMap::new(),
        }
    }
}

impl CounterState {
    fn validate(&self) -> Result<()> {
        if self.format != 1
            || self.epoch.data_loss_number < 0
            || self.epoch.configuration_number < 0
            || self.base_lsn < 0
            || self.committed < self.base_lsn
            || self.applied < self.committed
        {
            return Err(app_error("invalid counter storage format or progress"));
        }
        let mut previous = self.base_lsn;
        for (lsn, record) in &self.operations {
            if previous.checked_add(1) != Some(*lsn)
                || record.committed < 0
                || record.committed > *lsn
            {
                return Err(app_error("invalid counter operation history"));
            }
            previous = *lsn;
        }
        if previous != self.applied {
            return Err(app_error("counter history does not cover applied progress"));
        }
        Ok(())
    }

    fn value_at(&self, boundary: i64) -> Result<i64> {
        if boundary < self.base_lsn || boundary > self.applied {
            return Err(app_error("counter snapshot boundary is not retained"));
        }
        Ok(self
            .operations
            .range(..=boundary)
            .next_back()
            .map_or(self.base_value, |(_, record)| record.value))
    }

    fn progress(&self) -> DurableApplicationProgress {
        DurableApplicationProgress {
            applied_lsn: self.applied,
            committed_lsn: self.committed,
        }
    }
}

pub struct CounterStore {
    root: PathBuf,
    state: Mutex<CounterState>,
    changed: Notify,
}

impl CounterStore {
    /// Classify before opening. Unexpected or partial files reject unchanged.
    pub fn open(root: &Path) -> Result<(Self, bool)> {
        let fresh = !root.exists() || std::fs::read_dir(root).map_err(app_error)?.next().is_none();
        let state = if fresh {
            CounterState::default()
        } else {
            let bytes = std::fs::read(root.join("counter.json")).map_err(app_error)?;
            let state: CounterState = serde_json::from_slice(&bytes).map_err(app_error)?;
            state.validate()?;
            state
        };
        std::fs::create_dir_all(root).map_err(app_error)?;
        Ok((
            Self {
                root: root.to_owned(),
                state: Mutex::new(state),
                changed: Notify::new(),
            },
            fresh,
        ))
    }

    fn persist(&self, candidate: &CounterState) -> Result<()> {
        candidate.validate()?;
        let bytes = serde_json::to_vec(candidate).map_err(app_error)?;
        let temporary = self.root.join("counter.tmp");
        std::fs::write(&temporary, bytes).map_err(app_error)?;
        std::fs::File::open(&temporary)
            .and_then(|file| file.sync_all())
            .map_err(app_error)?;
        std::fs::rename(temporary, self.root.join("counter.json")).map_err(app_error)?;
        std::fs::File::open(&self.root)
            .and_then(|directory| directory.sync_all())
            .map_err(app_error)
    }

    fn authorize_open(&self, mode: OpenMode) -> Result<()> {
        if mode == OpenMode::Existing && !self.root.join("counter.json").is_file() {
            return Err(app_error(
                "established counter storage is missing metadata; refusing empty recovery",
            ));
        }
        self.persist(&self.state.lock().unwrap())
    }

    fn copy_directory(&self, id: &OperationId) -> PathBuf {
        let digest = Bytes::copy_from_slice(&Sha256::digest(id.as_str().as_bytes()));
        self.root.join("copy").join(format!("{digest:x}"))
    }

    fn install_snapshot(&self, bytes: &[u8]) -> Result<()> {
        let snapshot: CounterState = serde_json::from_slice(bytes).map_err(app_error)?;
        snapshot.validate()?;
        let mut state = self.state.lock().unwrap();
        if snapshot.committed < state.committed
            || (state.committed > 0
                && (state.committed < snapshot.base_lsn
                    || snapshot.value_at(state.committed)? != state.value_at(state.committed)?))
        {
            return Err(app_error("copy conflicts with acknowledged counter state"));
        }
        self.persist(&snapshot)?;
        *state = snapshot;
        self.changed.notify_waiters();
        if trace_counter() {
            eprintln!(
                "counter {} installed snapshot at {}",
                self.root.display(),
                state.committed
            );
        }
        Ok(())
    }
}

#[async_trait]
impl DurableState for CounterStore {
    async fn get_replication_operations(
        &self,
        from: i64,
        to: i64,
    ) -> Result<RetainedOperationStream> {
        let state = self.state.lock().unwrap();
        let operations = state
            .operations
            .iter()
            .filter(|(lsn, _)| **lsn >= from && **lsn <= to)
            .map(|(lsn, record)| {
                Ok(Operation {
                    lsn: *lsn,
                    committed_lsn: record.committed,
                    data: Bytes::copy_from_slice(&record.value.to_le_bytes()),
                })
            })
            .collect::<Vec<_>>();
        Ok(Box::pin(stream::iter(operations)))
    }

    async fn apply_copy_chunk(
        &self,
        id: &OperationId,
        sequence: u64,
        chunk: CopyChunk,
    ) -> Result<()> {
        let directory = self.copy_directory(id);
        std::fs::create_dir_all(&directory).map_err(app_error)?;
        let path = directory.join(format!("{sequence:020}.chunk"));
        if path.exists() {
            if std::fs::read(&path).map_err(app_error)? != chunk.data {
                return Err(app_error("counter copy sequence changed bytes"));
            }
        } else {
            let temporary = path.with_extension("tmp");
            std::fs::write(&temporary, &chunk.data).map_err(app_error)?;
            std::fs::File::open(&temporary)
                .and_then(|file| file.sync_all())
                .map_err(app_error)?;
            std::fs::rename(temporary, path).map_err(app_error)?;
        }
        for path in [&directory, &self.root.join("copy"), &self.root] {
            std::fs::File::open(path)
                .and_then(|directory| directory.sync_all())
                .map_err(app_error)?;
        }
        Ok(())
    }

    async fn verify_copy_chunk(
        &self,
        id: &OperationId,
        sequence: u64,
        chunk: &CopyChunk,
    ) -> Result<bool> {
        let path = self
            .copy_directory(id)
            .join(format!("{sequence:020}.chunk"));
        match std::fs::read(path) {
            Ok(bytes) => Ok(bytes == chunk.data),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(app_error(error)),
        }
    }

    async fn finish_copy(
        &self,
        id: &OperationId,
        up_to: i64,
        committed: i64,
    ) -> Result<DurableApplicationProgress> {
        let mut chunks = std::fs::read_dir(self.copy_directory(id))
            .map_err(app_error)?
            .collect::<std::io::Result<Vec<_>>>()
            .map_err(app_error)?;
        chunks.retain(|entry| entry.path().extension().is_some_and(|ext| ext == "chunk"));
        chunks.sort_by_key(|entry| entry.file_name());
        let mut bytes = Vec::new();
        for chunk in chunks {
            bytes.extend(std::fs::read(chunk.path()).map_err(app_error)?);
        }
        let value = decode(&bytes)?;
        let mut state = self.state.lock().unwrap();
        let candidate = CounterState {
            epoch: state.epoch,
            base_lsn: up_to,
            base_value: value,
            applied: up_to,
            committed,
            ..CounterState::default()
        };
        self.persist(&candidate)?;
        *state = candidate;
        self.changed.notify_waiters();
        Ok(state.progress())
    }

    async fn apply(&self, operation: Operation) -> Result<DurableApplicationProgress> {
        let value = decode(&operation.data)?;
        let mut state = self.state.lock().unwrap();
        if let Some(record) = state.operations.get(&operation.lsn) {
            if record.value != value || record.committed != operation.committed_lsn {
                return Err(app_error("counter LSN reused with different operation"));
            }
            return Ok(state.progress());
        }
        if state.applied.checked_add(1) != Some(operation.lsn) {
            return Err(app_error("counter operation is not contiguous"));
        }
        let mut candidate = state.clone();
        candidate.operations.insert(
            operation.lsn,
            Record {
                value,
                committed: operation.committed_lsn,
            },
        );
        candidate.applied = operation.lsn;
        candidate.committed = candidate.committed.max(operation.committed_lsn);
        self.persist(&candidate)?;
        *state = candidate;
        self.changed.notify_waiters();
        Ok(state.progress())
    }

    async fn durable_progress(&self) -> Result<DurableApplicationProgress> {
        Ok(self.state.lock().unwrap().progress())
    }

    async fn verify_applied(&self, operation: &Operation) -> Result<bool> {
        let value = decode(&operation.data)?;
        Ok(self
            .state
            .lock()
            .unwrap()
            .operations
            .get(&operation.lsn)
            .is_some_and(|record| {
                record.value == value && record.committed == operation.committed_lsn
            }))
    }

    async fn commit(&self, committed: i64) -> Result<DurableApplicationProgress> {
        let mut state = self.state.lock().unwrap();
        if committed < 0 || committed > state.applied {
            return Err(app_error("invalid counter commit boundary"));
        }
        let mut candidate = state.clone();
        candidate.committed = candidate.committed.max(committed);
        self.persist(&candidate)?;
        *state = candidate;
        self.changed.notify_waiters();
        Ok(state.progress())
    }
}

#[async_trait]
impl StateProvider for CounterStore {
    async fn update_epoch(&self, epoch: Epoch, _previous: i64) -> Result<()> {
        let mut state = self.state.lock().unwrap();
        if epoch < state.epoch {
            return Err(app_error("counter epoch regressed"));
        }
        let mut candidate = state.clone();
        candidate.epoch = epoch;
        self.persist(&candidate)?;
        *state = candidate;
        Ok(())
    }

    async fn last_committed_lsn(&self) -> Result<i64> {
        Ok(self.state.lock().unwrap().committed)
    }

    async fn get_copy_context(&self) -> Result<OperationDataStream> {
        if trace_counter() {
            eprintln!("counter {} get_copy_context", self.root.display());
        }
        Ok(Box::pin(stream::empty()))
    }

    async fn get_copy_state(
        &self,
        up_to: i64,
        mut context: OperationDataStream,
    ) -> Result<OperationDataStream> {
        if trace_counter() {
            eprintln!(
                "counter {} get_copy_state through {up_to}",
                self.root.display()
            );
        }
        if context.next().await.is_some() {
            return Err(app_error("counter does not accept copy context"));
        }
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            let snapshot = {
                let state = self.state.lock().unwrap();
                if up_to < 0 {
                    return Err(app_error("negative counter snapshot boundary"));
                }
                if up_to <= state.committed {
                    if up_to < state.base_lsn {
                        return Err(app_error("counter snapshot boundary is not retained"));
                    }
                    // Retain evidence needed to verify an older acknowledged target prefix.
                    let mut snapshot = state.clone();
                    snapshot.applied = up_to;
                    snapshot.committed = up_to;
                    snapshot.operations.retain(|lsn, _| *lsn <= up_to);
                    Some(snapshot)
                } else {
                    None
                }
            };
            if let Some(snapshot) = snapshot {
                if trace_counter() {
                    eprintln!(
                        "counter {} snapshot ready through {up_to}",
                        self.root.display()
                    );
                }
                let bytes = Bytes::from(serde_json::to_vec(&snapshot).map_err(app_error)?);
                return Ok(Box::pin(stream::iter([Ok(bytes)])));
            }
            changed.await;
        }
    }

    async fn on_data_loss(&self) -> Result<bool> {
        Ok(false)
    }
}

pub struct CounterService {
    store: Arc<CounterStore>,
    open_modes: Mutex<Vec<OpenMode>>,
    address: String,
    partition: RwLock<Option<StatefulServicePartition>>,
    state: RwLock<Option<Arc<dyn StateReplicator>>>,
    control: RwLock<Option<Arc<dyn Replicator>>>,
    stop: watch::Sender<bool>,
    consumers: AsyncMutex<JoinSet<Result<()>>>,
    writes: AsyncMutex<JoinSet<Result<()>>>,
    write_gate: Arc<AsyncMutex<()>>,
    role: Mutex<ReplicaRole>,
    drain: AsyncMutex<DrainOutcome>,
}

#[derive(Default)]
struct DrainOutcome {
    cancelled: bool,
    failures: Vec<String>,
}

impl DrainOutcome {
    fn record(&mut self, joined: std::result::Result<Result<()>, tokio::task::JoinError>) {
        match joined {
            Ok(Ok(())) => {}
            Ok(Err(RuntimeError::OperationCancelled)) => self.cancelled = true,
            Ok(Err(error)) => self.failures.push(error.to_string()),
            Err(error) => self.failures.push(error.to_string()),
        }
    }

    fn result(&self) -> Result<()> {
        if !self.failures.is_empty() {
            Err(app_error(self.failures.join("; ")))
        } else if self.cancelled {
            Err(RuntimeError::OperationCancelled)
        } else {
            Ok(())
        }
    }
}

impl CounterService {
    pub fn new(store: CounterStore, address: String) -> Self {
        Self {
            store: Arc::new(store),
            open_modes: Mutex::new(Vec::new()),
            address,
            partition: RwLock::new(None),
            state: RwLock::new(None),
            control: RwLock::new(None),
            stop: watch::channel(false).0,
            consumers: AsyncMutex::new(JoinSet::new()),
            writes: AsyncMutex::new(JoinSet::new()),
            write_gate: Arc::new(AsyncMutex::new(())),
            role: Mutex::new(ReplicaRole::None),
            drain: AsyncMutex::new(DrainOutcome::default()),
        }
    }

    pub fn open_modes(&self) -> Vec<OpenMode> {
        self.open_modes.lock().unwrap().clone()
    }

    pub async fn set(&self, value: i64) -> Result<i64> {
        let permit = self.write_gate.clone().lock_owned().await;
        let mut writes = self.writes.lock().await;
        if *self.stop.borrow() {
            return Err(RuntimeError::Closed);
        }
        if *self.role.lock().unwrap() != ReplicaRole::Primary {
            return Err(RuntimeError::NotPrimary);
        }
        let partition = self
            .partition
            .read()
            .await
            .clone()
            .ok_or(RuntimeError::NotOpen)?;
        let access = partition.get_write_status().await?;
        if access != AccessStatus::Granted {
            return Err(RuntimeError::WriteClosed(access));
        }
        let data = Bytes::copy_from_slice(&value.to_le_bytes());
        let admitted = self
            .state
            .read()
            .await
            .clone()
            .ok_or(RuntimeError::NotOpen)?
            .begin_replicate(data.clone())?;
        if trace_counter() {
            eprintln!(
                "counter {} admitted value {value} at {}",
                self.store.root.display(),
                admitted.lsn,
            );
        }
        let store = self.store.clone();
        let (outcome, observer) = oneshot::channel();
        writes.spawn(async move {
            let _permit = permit;
            let result = async {
                admitted.completion.await?;
                store
                    .apply(Operation {
                        lsn: admitted.lsn,
                        committed_lsn: admitted.lsn - 1,
                        data,
                    })
                    .await?;
                store.commit(admitted.lsn).await?;
                Ok(admitted.lsn)
            }
            .await;
            if trace_counter() {
                eprintln!(
                    "counter {} write {} settled {result:?}",
                    store.root.display(),
                    admitted.lsn,
                );
            }
            let cancelled = matches!(result, Err(RuntimeError::OperationCancelled));
            let error = result.as_ref().err().map(ToString::to_string);
            let _ = outcome.send(result);
            if cancelled {
                return Err(RuntimeError::OperationCancelled);
            }
            if let Some(error) = error {
                if let Err(report) = partition.report_fault(FaultType::Transient).await {
                    return Err(app_error(format!(
                        "{error}; fault reporting failed: {report}"
                    )));
                }
                return Err(app_error(error));
            }
            Ok(())
        });
        drop(writes);
        observer.await.map_err(app_error)?
    }

    pub async fn read(&self) -> Result<i64> {
        let partition = self
            .partition
            .read()
            .await
            .clone()
            .ok_or(RuntimeError::NotOpen)?;
        let access = partition.get_read_status().await?;
        if access != AccessStatus::Granted {
            return Err(RuntimeError::ReadClosed(access));
        }
        let state = self.store.state.lock().unwrap();
        state.value_at(state.committed)
    }

    /// Fixture diagnostic of service-owned durable state, not a serving read.
    pub fn committed_value(&self) -> Result<i64> {
        let state = self.store.state.lock().unwrap();
        state.value_at(state.committed)
    }

    pub async fn shutdown_and_drain(&self) -> Result<()> {
        match self.close().await {
            Err(RuntimeError::OperationCancelled) => {
                if self.read().await.is_ok() || self.set(0).await.is_ok() {
                    return Err(app_error("cancelled counter service still permits serving"));
                }
                Ok(())
            }
            result => result,
        }
    }

    async fn drain_owned_tasks(&self) -> Result<()> {
        let mut outcome = self.drain.lock().await;
        let mut tasks = self.consumers.lock().await;
        while let Some(joined) = tasks.join_next().await {
            outcome.record(joined);
        }
        let mut writes = self.writes.lock().await;
        while let Some(joined) = writes.join_next().await {
            outcome.record(joined);
        }
        outcome.result()
    }

    fn spawn_consumer(
        &self,
        tasks: &mut JoinSet<Result<()>>,
        mut stream: OperationStream,
        mut copied: watch::Receiver<bool>,
        copy_done: Option<watch::Sender<bool>>,
        partition: StatefulServicePartition,
    ) {
        let mut stop = self.stop.subscribe();
        let store = self.store.clone();
        let kind = if copy_done.is_some() {
            "copy"
        } else {
            "replication"
        };
        tasks.spawn(async move {
            tokio::select! {
                biased;
                _ = stop.wait_for(|stopped| *stopped) => Ok(()),
                result = async {
                    let wait_for_copy = copy_done.is_none() && !*copied.borrow();
                    if wait_for_copy && copied.wait_for(|done| *done).await.is_err() {
                        // The copy observer retains any fault; no copy completion
                        // or replication ACK is manufactured by its retirement.
                        return Ok(());
                    }
                    let mut ended = false;
                    loop {
                        let operation = match stream.get_operation().await {
                            Ok(Some(operation)) => operation,
                            Ok(None) => break,
                            Err(RuntimeError::Closed) => return Ok(()),
                            Err(error) => return Err(app_error(format!(
                                "counter {kind} get_operation: {error}",
                            ))),
                        };
                        if trace_counter() {
                            eprintln!(
                                "counter {} received {kind} {} end={}",
                                store.root.display(),
                                operation.lsn,
                                operation.end_of_stream,
                            );
                        }
                        let result = async {
                            if operation.end_of_stream {
                                ended = true;
                            } else if copy_done.is_some() {
                                store.install_snapshot(&operation.data)?;
                            } else {
                                store.apply(Operation {
                                    lsn: operation.lsn,
                                    committed_lsn: operation.lsn - 1,
                                    data: operation.data.clone(),
                                }).await?;
                                store.commit(operation.lsn).await?;
                            }
                            operation.acknowledge()
                        }.await;
                        if let Err(RuntimeError::Closed) = result {
                            return Ok(());
                        } else if let Err(error) = result {
                            let _ = stream.report_fault(error.to_string());
                            partition.report_fault(FaultType::Transient).await?;
                            return Err(error);
                        }
                    }
                    if ended && let Some(copy_done) = copy_done {
                        copy_done.send_replace(true);
                    }
                    Ok(())
                } => result,
            }
        });
    }
}

#[async_trait]
impl StatefulServiceReplica for CounterService {
    async fn open(self: Arc<Self>, context: OpenContext) -> Result<Arc<dyn Replicator>> {
        self.open_modes.lock().unwrap().push(context.mode);
        if *self.stop.borrow() {
            return Err(RuntimeError::Closed);
        }
        if let Some(control) = self.control.read().await.clone() {
            return Ok(control);
        }
        self.store.authorize_open(context.mode)?;
        let interfaces = context
            .partition
            .with_factory(Arc::new(DefaultReplicatorFactory::new()))
            .create_replicator(
                Some(self.store.clone()),
                Some(ReplicatorSettings {
                    replication_address: self.address.clone(),
                }),
            )
            .await?;
        let state = interfaces.state_replicator().ok_or(RuntimeError::NotOpen)?;
        *self.partition.write().await = Some(context.partition);
        *self.state.write().await = Some(state);
        let control = interfaces.replicator();
        *self.control.write().await = Some(control.clone());
        Ok(control)
    }

    async fn change_role(&self, role: ReplicaRole) -> Result<RoleChange> {
        let previous = *self.role.lock().unwrap();
        if trace_counter() {
            eprintln!(
                "counter {} role {previous:?} -> {role:?}",
                self.store.root.display(),
            );
        }
        if role == ReplicaRole::Primary || previous == ReplicaRole::Primary {
            let _drained = self.write_gate.lock().await;
        }
        if matches!(
            role,
            ReplicaRole::IdleSecondary | ReplicaRole::ActiveSecondary
        ) && !matches!(
            previous,
            ReplicaRole::IdleSecondary | ReplicaRole::ActiveSecondary
        ) {
            let state = self
                .state
                .read()
                .await
                .clone()
                .ok_or(RuntimeError::NotOpen)?;
            let partition = self
                .partition
                .read()
                .await
                .clone()
                .ok_or(RuntimeError::NotOpen)?;
            let copy = role == ReplicaRole::IdleSecondary || previous != ReplicaRole::Primary;
            let (copied, completion) = watch::channel(!copy);
            let mut tasks = self.consumers.lock().await;
            if copy {
                self.spawn_consumer(
                    &mut tasks,
                    state.copy_operations()?,
                    completion.clone(),
                    Some(copied),
                    partition.clone(),
                );
            }
            self.spawn_consumer(
                &mut tasks,
                state.replication_operations()?,
                completion,
                None,
                partition,
            );
        }
        *self.role.lock().unwrap() = role;
        Ok(RoleChange {
            service_address: (role == ReplicaRole::Primary).then(|| "in-process/counter".into()),
        })
    }

    async fn close(&self) -> Result<()> {
        self.stop.send_replace(true);
        self.state.write().await.take();
        self.control.write().await.take();
        self.partition.write().await.take();
        *self.role.lock().unwrap() = ReplicaRole::None;
        self.drain_owned_tasks().await
    }

    fn abort(&self) {
        self.stop.send_replace(true);
    }
}

#[cfg(feature = "harness")]
mod harness;
#[cfg(feature = "harness")]
pub use harness::CounterFactory;

#[cfg(all(test, feature = "harness"))]
mod journeys;

#[cfg(test)]
mod provider_tests {
    use super::*;

    #[test]
    fn typed_cancellation_cache_never_suppresses_application_errors() {
        let mut outcome = DrainOutcome::default();
        outcome.record(Ok(Err(RuntimeError::OperationCancelled)));
        assert!(matches!(
            outcome.result(),
            Err(RuntimeError::OperationCancelled)
        ));
        assert!(matches!(
            outcome.result(),
            Err(RuntimeError::OperationCancelled)
        ));
        outcome.record(Ok(Err(app_error(
            "operation cancelled; actual persistence failure",
        ))));
        assert!(
            matches!(outcome.result(), Err(RuntimeError::Application(error))
            if error.contains("actual persistence failure"))
        );
    }

    #[tokio::test]
    async fn owned_cleanup_accepts_only_typed_cancellation_and_retains_worker_failures()
    -> Result<()> {
        let root =
            tempfile::tempdir_in(std::env::current_dir().map_err(app_error)?).map_err(app_error)?;
        for failed in [false, true] {
            let (store, _) = CounterStore::open(&root.path().join(failed.to_string()))?;
            let service = CounterService::new(store, "http://127.0.0.1:0".into());
            let mut writes = service.writes.lock().await;
            writes.spawn(async { Err(RuntimeError::OperationCancelled) });
            if failed {
                writes.spawn(async {
                    Err(app_error(std::io::Error::new(
                        std::io::ErrorKind::Interrupted,
                        "actual storage interruption",
                    )))
                });
            }
            drop(writes);
            if failed {
                for _ in 0..2 {
                    assert!(matches!(service.shutdown_and_drain().await,
                        Err(RuntimeError::Application(error))
                            if error.contains("actual storage interruption")));
                }
            } else {
                assert!(matches!(
                    service.close().await,
                    Err(RuntimeError::OperationCancelled)
                ));
                service.shutdown_and_drain().await?;
                assert!(service.read().await.is_err());
                assert!(service.set(99).await.is_err());
            }
        }
        Ok(())
    }

    #[test]
    fn existing_open_never_initializes_missing_service_metadata() -> Result<()> {
        let root =
            tempfile::tempdir_in(std::env::current_dir().map_err(app_error)?).map_err(app_error)?;
        let (store, fresh) = CounterStore::open(root.path())?;
        assert!(fresh);
        assert!(store.authorize_open(OpenMode::Existing).is_err());
        assert!(
            std::fs::read_dir(root.path())
                .map_err(app_error)?
                .next()
                .is_none()
        );
        store.authorize_open(OpenMode::New)?;
        drop(store);
        let (store, fresh) = CounterStore::open(root.path())?;
        assert!(!fresh);
        store.authorize_open(OpenMode::Existing)?;
        assert_eq!(store.state.lock().unwrap().committed, 0);
        Ok(())
    }

    #[tokio::test]
    async fn own_provider_persists_reopens_and_builds_a_frozen_snapshot() -> Result<()> {
        let source =
            tempfile::tempdir_in(std::env::current_dir().map_err(app_error)?).map_err(app_error)?;
        let (store, fresh) = CounterStore::open(source.path())?;
        assert!(fresh);
        store
            .apply(Operation {
                lsn: 1,
                committed_lsn: 0,
                data: Bytes::copy_from_slice(&17_i64.to_le_bytes()),
            })
            .await?;
        store.commit(1).await?;
        let mut snapshot = store.get_copy_state(1, Box::pin(stream::empty())).await?;
        let target =
            tempfile::tempdir_in(std::env::current_dir().map_err(app_error)?).map_err(app_error)?;
        let (copy, _) = CounterStore::open(target.path())?;
        let bytes = snapshot.next().await.unwrap()?;
        assert!(snapshot.next().await.is_none());
        copy.install_snapshot(&bytes)?;
        assert_eq!(copy.state.lock().unwrap().value_at(1)?, 17);
        let (reopened_copy, fresh) = CounterStore::open(target.path())?;
        assert!(!fresh);
        assert_eq!(reopened_copy.last_committed_lsn().await?, 1);
        assert_eq!(reopened_copy.state.lock().unwrap().value_at(1)?, 17);
        drop(store);
        let (reopened, fresh) = CounterStore::open(source.path())?;
        assert!(!fresh);
        assert_eq!(reopened.durable_progress().await?.committed_lsn, 1);
        assert_eq!(reopened.state.lock().unwrap().value_at(1)?, 17);
        Ok(())
    }

    #[tokio::test]
    async fn native_copy_snapshot_is_frozen_and_malformed_copy_cannot_mutate_state() -> Result<()> {
        let root =
            tempfile::tempdir_in(std::env::current_dir().map_err(app_error)?).map_err(app_error)?;
        let (store, _) = CounterStore::open(root.path())?;
        store
            .apply(Operation {
                lsn: 1,
                committed_lsn: 0,
                data: Bytes::copy_from_slice(&17_i64.to_le_bytes()),
            })
            .await?;
        store.commit(1).await?;
        let mut snapshot = store.get_copy_state(1, Box::pin(stream::empty())).await?;
        store
            .apply(Operation {
                lsn: 2,
                committed_lsn: 1,
                data: Bytes::copy_from_slice(&23_i64.to_le_bytes()),
            })
            .await?;
        store.commit(2).await?;
        let bytes = snapshot.next().await.unwrap()?;
        let snapshot: CounterState = serde_json::from_slice(&bytes).map_err(app_error)?;
        assert_eq!(snapshot.committed, 1);
        assert_eq!(snapshot.value_at(1)?, 17);
        let before = std::fs::read(root.path().join("counter.json")).map_err(app_error)?;
        assert!(store.install_snapshot(b"invalid snapshot").is_err());
        assert!(
            store.install_snapshot(&bytes).is_err(),
            "copy rolled back acknowledged state"
        );
        assert_eq!(
            std::fs::read(root.path().join("counter.json")).map_err(app_error)?,
            before
        );
        Ok(())
    }

    #[tokio::test]
    async fn native_snapshot_advances_retained_secondary_only_with_matching_acknowledged_prefix()
    -> Result<()> {
        let source =
            tempfile::tempdir_in(std::env::current_dir().map_err(app_error)?).map_err(app_error)?;
        let target =
            tempfile::tempdir_in(std::env::current_dir().map_err(app_error)?).map_err(app_error)?;
        let (store, _) = CounterStore::open(source.path())?;
        let (copy, _) = CounterStore::open(target.path())?;
        for (lsn, value) in [(1, 17_i64), (2, 23)] {
            store
                .apply(Operation {
                    lsn,
                    committed_lsn: lsn - 1,
                    data: Bytes::copy_from_slice(&value.to_le_bytes()),
                })
                .await?;
            store.commit(lsn).await?;
            let mut stream = store.get_copy_state(lsn, Box::pin(stream::empty())).await?;
            let bytes = stream.next().await.unwrap()?;
            assert!(stream.next().await.is_none());
            if lsn == 2 {
                let mut crossed: CounterState =
                    serde_json::from_slice(&bytes).map_err(app_error)?;
                crossed.operations.get_mut(&1).unwrap().value = 99;
                let before =
                    std::fs::read(target.path().join("counter.json")).map_err(app_error)?;
                assert!(
                    copy.install_snapshot(&serde_json::to_vec(&crossed).map_err(app_error)?)
                        .is_err()
                );
                assert_eq!(
                    std::fs::read(target.path().join("counter.json")).map_err(app_error)?,
                    before
                );
                assert_eq!(copy.last_committed_lsn().await?, 1);
            }
            copy.install_snapshot(&bytes)?;
            assert_eq!(copy.last_committed_lsn().await?, lsn);
            assert_eq!(copy.state.lock().unwrap().value_at(1)?, 17);
            assert_eq!(copy.state.lock().unwrap().value_at(lsn)?, value);
        }
        let (reopened, fresh) = CounterStore::open(target.path())?;
        assert!(!fresh);
        assert_eq!(reopened.last_committed_lsn().await?, 2);
        assert_eq!(reopened.state.lock().unwrap().value_at(1)?, 17);
        assert_eq!(reopened.state.lock().unwrap().value_at(2)?, 23);
        Ok(())
    }

    #[test]
    fn incompatible_application_state_rejects_without_mutation() -> Result<()> {
        let root =
            tempfile::tempdir_in(std::env::current_dir().map_err(app_error)?).map_err(app_error)?;
        let path = root.path().join("counter.json");
        std::fs::write(&path, b"incompatible provider").map_err(app_error)?;
        assert!(CounterStore::open(root.path()).is_err());
        assert_eq!(
            std::fs::read(path).map_err(app_error)?,
            b"incompatible provider"
        );
        Ok(())
    }

    #[tokio::test]
    async fn malformed_operation_does_not_publish_progress() -> Result<()> {
        let root =
            tempfile::tempdir_in(std::env::current_dir().map_err(app_error)?).map_err(app_error)?;
        let (store, _) = CounterStore::open(root.path())?;
        assert!(
            store
                .apply(Operation {
                    lsn: 1,
                    committed_lsn: 0,
                    data: Bytes::from_static(b"invalid"),
                })
                .await
                .is_err()
        );
        assert_eq!(
            store.durable_progress().await?,
            DurableApplicationProgress::default()
        );
        assert!(!root.path().join("counter.json").exists());
        Ok(())
    }
}
