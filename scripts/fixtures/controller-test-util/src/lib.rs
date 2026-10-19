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
use kuberic_runtime::protocol::types::{AccessStatus, Epoch, OperationId, ReplicaRole};
use kuberic_runtime::replicator::stream::OperationStream;
use kuberic_runtime::replicator::{
    DefaultReplicatorFactory, Replicator, ReplicatorSettings, StateReplicator,
    StatefulServicePartition,
};
use kuberic_runtime::{Result, RuntimeError, StateProvider, StatefulServiceReplica};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::{Mutex as AsyncMutex, RwLock, watch};
use tokio::task::JoinSet;

fn app_error(error: impl std::fmt::Display) -> RuntimeError {
    RuntimeError::Application(error.to_string())
}

fn decode(bytes: &[u8]) -> Result<i64> {
    let value: [u8; 8] = bytes
        .try_into()
        .map_err(|_| app_error("counter operation must contain exactly eight bytes"))?;
    Ok(i64::from_le_bytes(value))
}

#[derive(Clone, Serialize, Deserialize)]
struct Record {
    value: i64,
    committed: i64,
}

#[derive(Clone, Serialize, Deserialize)]
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

    fn copy_directory(&self, id: &OperationId) -> PathBuf {
        let digest = Bytes::copy_from_slice(&Sha256::digest(id.as_str().as_bytes()));
        self.root.join("copy").join(format!("{digest:x}"))
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
        Ok(Box::pin(stream::empty()))
    }

    async fn get_copy_state(
        &self,
        up_to: i64,
        mut context: OperationDataStream,
    ) -> Result<OperationDataStream> {
        if context.next().await.is_some() {
            return Err(app_error("counter does not accept copy context"));
        }
        let state = self.state.lock().unwrap();
        if up_to > state.committed {
            return Err(app_error("counter snapshot is not committed"));
        }
        let bytes = Bytes::copy_from_slice(&state.value_at(up_to)?.to_le_bytes());
        Ok(Box::pin(stream::iter([Ok(bytes)])))
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
        }
    }

    pub fn open_modes(&self) -> Vec<OpenMode> {
        self.open_modes.lock().unwrap().clone()
    }

    pub async fn set(&self, value: i64) -> Result<i64> {
        self.state
            .read()
            .await
            .clone()
            .ok_or(RuntimeError::NotOpen)?
            .replicate(Bytes::copy_from_slice(&value.to_le_bytes()))
            .await
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

    pub async fn shutdown_and_drain(&self) -> Result<()> {
        self.close().await?;
        let mut tasks = self.consumers.lock().await;
        let mut failures = Vec::new();
        while let Some(joined) = tasks.join_next().await {
            match joined {
                Ok(Ok(())) => {}
                Ok(Err(error)) => failures.push(error.to_string()),
                Err(error) => failures.push(error.to_string()),
            }
        }
        if failures.is_empty() {
            Ok(())
        } else {
            Err(app_error(failures.join("; ")))
        }
    }

    fn spawn_consumer(&self, tasks: &mut JoinSet<Result<()>>, mut stream: OperationStream) {
        let mut stop = self.stop.subscribe();
        let store = self.store.clone();
        tasks.spawn(async move {
            tokio::select! {
                biased;
                _ = stop.wait_for(|stopped| *stopped) => Ok(()),
                result = async {
                    while let Some(operation) = stream.get_operation().await? {
                        match operation.apply_to(store.as_ref()).await {
                            Ok(progress) => { let _ = operation.acknowledge(progress); }
                            Err(error) => { let _ = operation.reject(error); }
                        }
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
        let mut tasks = self.consumers.lock().await;
        if *self.stop.borrow() {
            return Err(RuntimeError::Closed);
        }
        if let Some(control) = self.control.read().await.clone() {
            return Ok(control);
        }
        let interfaces = context
            .partition
            .with_factory(Arc::new(DefaultReplicatorFactory::new(self.store.clone())))
            .create_replicator(
                Some(self.store.clone()),
                Some(ReplicatorSettings {
                    replication_address: self.address.clone(),
                }),
            )
            .await?;
        let state = interfaces.state_replicator().ok_or(RuntimeError::NotOpen)?;
        self.spawn_consumer(&mut tasks, state.get_replication_stream().await?);
        self.spawn_consumer(&mut tasks, state.get_copy_stream().await?);
        *self.partition.write().await = Some(context.partition);
        *self.state.write().await = Some(state);
        let control = interfaces.replicator();
        *self.control.write().await = Some(control.clone());
        Ok(control)
    }

    async fn change_role(&self, role: ReplicaRole) -> Result<RoleChange> {
        Ok(RoleChange {
            service_address: (role == ReplicaRole::Primary).then(|| "in-process/counter".into()),
        })
    }

    async fn close(&self) -> Result<()> {
        self.stop.send_replace(true);
        self.state.write().await.take();
        self.control.write().await.take();
        self.partition.write().await.take();
        Ok(())
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

    #[tokio::test]
    async fn own_provider_persists_reopens_and_builds_a_frozen_snapshot() -> Result<()> {
        let source = tempfile::tempdir().map_err(app_error)?;
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
        let target = tempfile::tempdir().map_err(app_error)?;
        let (copy, _) = CounterStore::open(target.path())?;
        let build = OperationId::new("independent-counter-build");
        let mut sequence = 0;
        while let Some(data) = snapshot.next().await {
            let chunk = CopyChunk { data: data? };
            copy.apply_copy_chunk(&build, sequence, chunk.clone())
                .await?;
            assert!(copy.verify_copy_chunk(&build, sequence, &chunk).await?);
            sequence += 1;
        }
        copy.finish_copy(&build, 1, 1).await?;
        assert_eq!(copy.state.lock().unwrap().value_at(1)?, 17);
        drop(store);
        let (reopened, fresh) = CounterStore::open(source.path())?;
        assert!(!fresh);
        assert_eq!(reopened.durable_progress().await?.committed_lsn, 1);
        assert_eq!(reopened.state.lock().unwrap().value_at(1)?, 17);
        Ok(())
    }

    #[test]
    fn incompatible_application_state_rejects_without_mutation() -> Result<()> {
        let root = tempfile::tempdir().map_err(app_error)?;
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
        let root = tempfile::tempdir().map_err(app_error)?;
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
