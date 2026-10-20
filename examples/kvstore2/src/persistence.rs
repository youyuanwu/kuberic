use std::collections::BTreeMap;
use std::io::Write;
use std::ops::Bound::{Excluded, Included};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Mutex;
#[cfg(test)]
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use async_trait::async_trait;
use bytes::Bytes;
use futures::{Stream, stream};
use kuberic_runtime::application::{
    CopyChunk, DurableApplicationAck, DurableApplicationProgress, Operation,
};
use kuberic_runtime::engine::{DurableState, RetainedOperationStream};
use kuberic_runtime::protocol::types::{Epoch, OperationId};
use kuberic_runtime::{Result, RuntimeError};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum Mutation {
    Put { key: String, value: String },
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct PersistedState {
    values: BTreeMap<String, String>,
    #[serde(default)]
    base_values: BTreeMap<String, String>,
    #[serde(default)]
    base_lsn: i64,
    operations: BTreeMap<i64, OperationRecord>,
    applied_lsn: i64,
    committed_lsn: i64,
    epoch: Epoch,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct OperationRecord {
    committed_lsn: i64,
    data: Vec<u8>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct NativeSnapshot {
    version: u32,
    lsn: i64,
    values: BTreeMap<String, String>,
}

pub struct KvPersistence {
    root: PathBuf,
    state_path: PathBuf,
    state: Mutex<PersistedState>,
    #[cfg(feature = "testing")]
    apply_pause: Mutex<Option<ApplyPause>>,
    #[cfg(test)]
    fail_next_persist: AtomicBool,
    #[cfg(test)]
    stream_calls: (AtomicU64, AtomicU64),
}

#[cfg(feature = "testing")]
struct ApplyPause {
    entered: tokio::sync::oneshot::Sender<()>,
    release: tokio::sync::oneshot::Receiver<bool>,
}

/// One-shot test gate after a durable mutation is visible, before apply returns.
/// Dropping the gate cancels the paused apply rather than detaching work.
#[cfg(feature = "testing")]
#[allow(dead_code)] // The binary compiles this module too; integration tests use the library.
pub struct ApplyGate {
    entered: tokio::sync::oneshot::Receiver<()>,
    release: tokio::sync::oneshot::Sender<bool>,
}

#[cfg(feature = "testing")]
#[allow(dead_code)]
impl ApplyGate {
    pub async fn entered(&mut self) {
        (&mut self.entered)
            .await
            .expect("apply did not enter the gate");
    }

    pub fn release(self, fail: bool) {
        let _ = self.release.send(fail);
    }

    pub async fn cancelled(&mut self) {
        self.release.closed().await;
    }
}

impl KvPersistence {
    #[cfg(feature = "testing")]
    #[allow(dead_code)]
    pub fn pause_next_apply(&self) -> ApplyGate {
        let (entered, observed) = tokio::sync::oneshot::channel();
        let (release, resumed) = tokio::sync::oneshot::channel();
        let mut pause = self.apply_pause.lock().unwrap();
        assert!(pause.is_none(), "an apply gate is already armed");
        *pause = Some(ApplyPause {
            entered,
            release: resumed,
        });
        ApplyGate {
            entered: observed,
            release,
        }
    }

    #[cfg(test)]
    pub(crate) fn fail_next_persist_for_test(&self) {
        self.fail_next_persist.store(true, Ordering::SeqCst);
    }

    #[cfg(test)]
    pub(crate) fn stream_call_counts(&self) -> (u64, u64) {
        (
            self.stream_calls.0.load(Ordering::Acquire),
            self.stream_calls.1.load(Ordering::Acquire),
        )
    }

    pub fn is_fresh_empty(root: impl AsRef<Path>) -> Result<bool> {
        let root = root.as_ref();
        if !root.exists() {
            return Ok(true);
        }
        if !root.is_dir() {
            return Err(RuntimeError::Application(
                "application data root is not a directory".into(),
            ));
        }
        Ok(std::fs::read_dir(root)
            .map_err(|error| RuntimeError::Application(error.to_string()))?
            .next()
            .is_none())
    }

    pub fn open(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref().to_path_buf();
        std::fs::create_dir_all(&root)
            .map_err(|error| RuntimeError::Application(error.to_string()))?;
        let state_path = root.join("state.json");
        let state = match std::fs::read(&state_path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map_err(|error| RuntimeError::Application(error.to_string()))?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => PersistedState::default(),
            Err(error) => return Err(RuntimeError::Application(error.to_string())),
        };
        Ok(Self {
            root,
            state_path,
            state: Mutex::new(state),
            #[cfg(feature = "testing")]
            apply_pause: Mutex::new(None),
            #[cfg(test)]
            fail_next_persist: AtomicBool::new(false),
            #[cfg(test)]
            stream_calls: (AtomicU64::new(0), AtomicU64::new(0)),
        })
    }

    pub fn get(&self, key: &str) -> Option<String> {
        self.state.lock().unwrap().values.get(key).cloned()
    }

    pub fn snapshot_at(&self, up_to_lsn: i64) -> Result<BTreeMap<String, String>> {
        let state = self.state.lock().unwrap();
        Self::values_at(&state, up_to_lsn)
    }

    fn values_at(state: &PersistedState, up_to_lsn: i64) -> Result<BTreeMap<String, String>> {
        if up_to_lsn < state.base_lsn {
            return Err(RuntimeError::Application(format!(
                "snapshot boundary {up_to_lsn} predates retained base {}",
                state.base_lsn
            )));
        }
        let mut values = state.base_values.clone();
        for record in state
            .operations
            .range((Excluded(state.base_lsn), Included(up_to_lsn)))
            .map(|(_, record)| record)
        {
            match serde_json::from_slice::<Mutation>(&record.data)
                .map_err(|error| RuntimeError::Application(error.to_string()))?
            {
                Mutation::Put { key, value } => {
                    values.insert(key, value);
                }
            }
        }
        Ok(values)
    }

    fn mutation_candidate(
        state: &PersistedState,
        operation: &Operation,
        mutation: Mutation,
    ) -> PersistedState {
        let mut candidate = state.clone();
        match mutation {
            Mutation::Put { key, value } => {
                candidate.values.insert(key, value);
            }
        }
        candidate.operations.insert(
            operation.lsn,
            OperationRecord {
                committed_lsn: operation.committed_lsn,
                data: operation.data.to_vec(),
            },
        );
        candidate.applied_lsn = candidate.applied_lsn.max(operation.lsn);
        candidate.committed_lsn = candidate.committed_lsn.max(operation.committed_lsn);
        candidate
    }
}

#[allow(
    dead_code,
    reason = "private V1 companion is compiled in the library, not the binary"
)]
impl KvPersistence {
    pub(crate) fn native_recover(&self) -> Result<()> {
        let mut state = self.state.lock().unwrap();
        if state.base_lsn < 0
            || state.base_lsn > state.committed_lsn
            || state.committed_lsn > state.applied_lsn
            || state
                .operations
                .keys()
                .try_fold(state.base_lsn, |previous, &actual| {
                    (previous.checked_add(1) == Some(actual)).then_some(actual)
                })
                != Some(state.applied_lsn)
            || Self::values_at(&state, state.applied_lsn)? != state.values
        {
            return Err(RuntimeError::Application(
                "invalid native KV persisted prefix".into(),
            ));
        }
        if state.applied_lsn != state.committed_lsn {
            let mut candidate = state.clone();
            candidate.values = Self::values_at(&state, state.committed_lsn)?;
            candidate
                .operations
                .retain(|lsn, _| *lsn <= state.committed_lsn);
            candidate.applied_lsn = candidate.committed_lsn;
            self.persist(&candidate)?;
            *state = candidate;
        }
        Ok(())
    }

    pub(crate) fn native_progress(&self) -> i64 {
        self.state.lock().unwrap().committed_lsn
    }

    pub(crate) fn native_prepare(&self, lsn: i64, data: Bytes, commit: bool) -> Result<()> {
        let mutation: Mutation = serde_json::from_slice(&data)
            .map_err(|error| RuntimeError::Application(error.to_string()))?;
        let mut state = self.state.lock().unwrap();
        if state.applied_lsn.checked_add(1) != Some(lsn) {
            return Err(RuntimeError::Application(
                "native KV preparation is not the next LSN".into(),
            ));
        }
        let mut candidate = Self::mutation_candidate(
            &state,
            &Operation {
                lsn,
                committed_lsn: 0,
                data,
            },
            mutation,
        );
        if commit {
            if state.committed_lsn.checked_add(1) != Some(lsn) {
                return Err(RuntimeError::Application(
                    "native KV secondary commit would skip a prepare".into(),
                ));
            }
            candidate.committed_lsn = lsn;
        }
        self.persist(&candidate)?;
        *state = candidate;
        Ok(())
    }

    pub(crate) fn native_commit(&self, lsn: i64) -> Result<()> {
        let mut state = self.state.lock().unwrap();
        if state.committed_lsn.checked_add(1) != Some(lsn) || lsn > state.applied_lsn {
            return Err(RuntimeError::Application(
                "native KV local commit is not ordered".into(),
            ));
        }
        let mut candidate = state.clone();
        candidate.committed_lsn = lsn;
        self.persist(&candidate)?;
        *state = candidate;
        Ok(())
    }

    pub(crate) fn native_get(&self, key: &str) -> Result<Option<String>> {
        let state = self.state.lock().unwrap();
        Ok(Self::values_at(&state, state.committed_lsn)?
            .get(key)
            .cloned())
    }

    pub(crate) fn native_snapshot(&self, lsn: i64) -> Result<Bytes> {
        let state = self.state.lock().unwrap();
        if lsn > state.committed_lsn {
            return Err(RuntimeError::Application(
                "native KV snapshot is ahead of local commitment".into(),
            ));
        }
        serde_json::to_vec(&NativeSnapshot {
            version: 1,
            lsn,
            values: Self::values_at(&state, lsn)?,
        })
        .map(Bytes::from)
        .map_err(|error| RuntimeError::Application(error.to_string()))
    }

    pub(crate) fn native_install(&self, data: Bytes) -> Result<i64> {
        let snapshot: NativeSnapshot = serde_json::from_slice(&data)
            .map_err(|error| RuntimeError::Application(error.to_string()))?;
        let mut state = self.state.lock().unwrap();
        if snapshot.version != 1
            || snapshot.lsn < 0
            || snapshot.lsn < state.committed_lsn
            || (snapshot.lsn == state.committed_lsn
                && snapshot.values != Self::values_at(&state, state.committed_lsn)?)
        {
            return Err(RuntimeError::Application(
                "invalid or regressing native KV snapshot".into(),
            ));
        }
        let candidate = PersistedState {
            values: snapshot.values.clone(),
            base_values: snapshot.values,
            base_lsn: snapshot.lsn,
            operations: BTreeMap::new(),
            applied_lsn: snapshot.lsn,
            committed_lsn: snapshot.lsn,
            epoch: state.epoch,
        };
        self.persist(&candidate)?;
        *state = candidate;
        Ok(snapshot.lsn)
    }
}

impl KvPersistence {
    pub fn encode_put(key: String, value: String) -> Result<Bytes> {
        serde_json::to_vec(&Mutation::Put { key, value })
            .map(Bytes::from)
            .map_err(|error| RuntimeError::Application(error.to_string()))
    }

    pub fn update_epoch(&self, epoch: Epoch) -> Result<()> {
        let mut state = self.state.lock().unwrap();
        if epoch < state.epoch {
            return Err(RuntimeError::AuthorityMismatch(
                "application epoch regressed".into(),
            ));
        }
        let mut candidate = state.clone();
        candidate.epoch = epoch;
        self.persist(&candidate)?;
        *state = candidate;
        Ok(())
    }

    fn persist(&self, state: &PersistedState) -> Result<()> {
        #[cfg(test)]
        if self.fail_next_persist.swap(false, Ordering::SeqCst) {
            return Err(RuntimeError::Application(
                "injected persistence failure".into(),
            ));
        }
        let temporary = self.state_path.with_extension("json.tmp");
        let bytes = serde_json::to_vec(state)
            .map_err(|error| RuntimeError::Application(error.to_string()))?;
        std::fs::write(&temporary, bytes)
            .map_err(|error| RuntimeError::Application(error.to_string()))?;
        let file = std::fs::OpenOptions::new()
            .read(true)
            .open(&temporary)
            .map_err(|error| RuntimeError::Application(error.to_string()))?;
        file.sync_all()
            .map_err(|error| RuntimeError::Application(error.to_string()))?;
        std::fs::rename(&temporary, &self.state_path)
            .map_err(|error| RuntimeError::Application(error.to_string()))?;
        let directory = std::fs::File::open(&self.root)
            .map_err(|error| RuntimeError::Application(error.to_string()))?;
        directory
            .sync_all()
            .map_err(|error| RuntimeError::Application(error.to_string()))
    }

    fn copy_chunk_path(&self, build_id: &OperationId, sequence: u64) -> PathBuf {
        self.root
            .join("copy")
            .join(build_id.as_str())
            .join(format!("{sequence:020}.chunk"))
    }

    fn copy_chunk_staging_path(&self, build_id: &OperationId, sequence: u64) -> PathBuf {
        self.root
            .join("copy")
            .join(build_id.as_str())
            .join(format!("{sequence:020}.chunk.tmp"))
    }
}

#[async_trait]
impl DurableState for KvPersistence {
    async fn get_replication_operations(
        &self,
        from_lsn: i64,
        to_lsn: i64,
    ) -> Result<RetainedOperationStream> {
        if from_lsn > to_lsn {
            return Ok(Box::pin(stream::empty()));
        }
        let operations = self
            .state
            .lock()
            .unwrap()
            .operations
            .range(from_lsn..=to_lsn)
            .map(|(lsn, record)| {
                Ok(Operation {
                    lsn: *lsn,
                    committed_lsn: record.committed_lsn,
                    data: Bytes::copy_from_slice(&record.data),
                })
            })
            .collect::<Vec<_>>();
        Ok(Box::pin(stream::iter(operations)))
    }

    async fn apply_copy_chunk(
        &self,
        build_id: &OperationId,
        sequence: u64,
        chunk: CopyChunk,
    ) -> Result<()> {
        let path = self.copy_chunk_path(build_id, sequence);
        let staging = self.copy_chunk_staging_path(build_id, sequence);
        let parent = path
            .parent()
            .ok_or_else(|| RuntimeError::Application("copy chunk path has no parent".into()))?;
        std::fs::create_dir_all(parent)
            .map_err(|error| RuntimeError::Application(error.to_string()))?;
        if path.is_file() {
            if std::fs::read(&path).map_err(|error| RuntimeError::Application(error.to_string()))?
                != chunk.data
            {
                return Err(RuntimeError::AuthorityMismatch(
                    "copy sequence was reused with different bytes".into(),
                ));
            }
            std::fs::File::open(&path)
                .and_then(|file| file.sync_all())
                .map_err(|error| RuntimeError::Application(error.to_string()))?;
        } else {
            match std::fs::remove_file(&staging) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(RuntimeError::Application(error.to_string())),
            }
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&staging)
                .map_err(|error| RuntimeError::Application(error.to_string()))?;
            file.write_all(&chunk.data)
                .map_err(|error| RuntimeError::Application(error.to_string()))?;
            file.sync_all()
                .map_err(|error| RuntimeError::Application(error.to_string()))?;
            drop(file);
            std::fs::rename(&staging, &path)
                .map_err(|error| RuntimeError::Application(error.to_string()))?;
        }
        let mut directory = Some(parent);
        while let Some(path) = directory {
            std::fs::File::open(path)
                .and_then(|directory| directory.sync_all())
                .map_err(|error| RuntimeError::Application(error.to_string()))?;
            if path == self.root {
                break;
            }
            directory = path.parent();
        }
        Ok(())
    }

    async fn verify_copy_chunk(
        &self,
        build_id: &OperationId,
        sequence: u64,
        chunk: &CopyChunk,
    ) -> Result<bool> {
        let path = self.copy_chunk_path(build_id, sequence);
        Ok(path.is_file()
            && std::fs::read(path).map_err(|error| RuntimeError::Application(error.to_string()))?
                == chunk.data)
    }

    async fn finish_copy(
        &self,
        build_id: &OperationId,
        up_to_lsn: i64,
        committed_lsn: i64,
    ) -> Result<DurableApplicationProgress> {
        let directory = self.root.join("copy").join(build_id.as_str());
        let mut paths = std::fs::read_dir(&directory)
            .map_err(|error| RuntimeError::Application(error.to_string()))?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|error| RuntimeError::Application(error.to_string()))?;
        paths.retain(|entry| {
            entry
                .path()
                .extension()
                .is_some_and(|extension| extension == "chunk")
        });
        paths.sort_by_key(|entry| entry.file_name());
        let mut bytes = Vec::new();
        for entry in paths {
            bytes.extend(
                std::fs::read(entry.path())
                    .map_err(|error| RuntimeError::Application(error.to_string()))?,
            );
        }
        let values = if bytes.is_empty() {
            BTreeMap::new()
        } else {
            serde_json::from_slice(&bytes)
                .map_err(|error| RuntimeError::Application(error.to_string()))?
        };
        let mut state = self.state.lock().unwrap();
        let mut candidate = state.clone();
        candidate.values = values.clone();
        candidate.base_values = values;
        candidate.base_lsn = up_to_lsn;
        candidate.operations.clear();
        candidate.applied_lsn = up_to_lsn;
        candidate.committed_lsn = committed_lsn;
        self.persist(&candidate)?;
        *state = candidate;
        Ok(DurableApplicationProgress {
            applied_lsn: up_to_lsn,
            committed_lsn,
        })
    }

    async fn apply(&self, operation: Operation) -> Result<DurableApplicationAck> {
        #[cfg(test)]
        self.stream_calls.0.fetch_add(1, Ordering::AcqRel);
        let mutation: Mutation = serde_json::from_slice(&operation.data)
            .map_err(|error| RuntimeError::Application(error.to_string()))?;
        let progress = {
            let mut state = self.state.lock().unwrap();
            if let Some(existing) = state.operations.get(&operation.lsn) {
                if existing.data == operation.data
                    && existing.committed_lsn == operation.committed_lsn
                {
                    return Ok(DurableApplicationProgress {
                        applied_lsn: state.applied_lsn,
                        committed_lsn: state.committed_lsn,
                    });
                }
                return Err(RuntimeError::AuthorityMismatch(
                    "LSN was reused with different application data".into(),
                ));
            }
            let candidate = Self::mutation_candidate(&state, &operation, mutation);
            self.persist(&candidate)?;
            *state = candidate;
            DurableApplicationProgress {
                applied_lsn: state.applied_lsn,
                committed_lsn: state.committed_lsn,
            }
        };
        #[cfg(feature = "testing")]
        {
            let pause = self.apply_pause.lock().unwrap().take();
            if let Some(pause) = pause {
                let _ = pause.entered.send(());
                if pause
                    .release
                    .await
                    .map_err(|_| RuntimeError::OperationCancelled)?
                {
                    return Err(RuntimeError::Application(
                        "injected failure after visible apply".into(),
                    ));
                }
            }
        }
        Ok(progress)
    }

    async fn durable_progress(&self) -> Result<DurableApplicationProgress> {
        let state = self.state.lock().unwrap();
        Ok(DurableApplicationProgress {
            applied_lsn: state.applied_lsn,
            committed_lsn: state.committed_lsn,
        })
    }

    async fn verify_applied(&self, operation: &Operation) -> Result<bool> {
        Ok(self
            .state
            .lock()
            .unwrap()
            .operations
            .get(&operation.lsn)
            .is_some_and(|record| {
                record.data == operation.data && record.committed_lsn == operation.committed_lsn
            }))
    }

    async fn commit(&self, committed_lsn: i64) -> Result<DurableApplicationProgress> {
        #[cfg(test)]
        self.stream_calls.1.fetch_add(1, Ordering::AcqRel);
        let mut state = self.state.lock().unwrap();
        if committed_lsn > state.applied_lsn {
            return Err(RuntimeError::Application(
                "cannot commit beyond applied progress".into(),
            ));
        }
        let mut candidate = state.clone();
        candidate.committed_lsn = candidate.committed_lsn.max(committed_lsn);
        self.persist(&candidate)?;
        *state = candidate;
        Ok(DurableApplicationProgress {
            applied_lsn: state.applied_lsn,
            committed_lsn: state.committed_lsn,
        })
    }
}

pub type SnapshotStream = Pin<Box<dyn Stream<Item = Result<Bytes>> + Send>>;

pub fn snapshot_stream(values: BTreeMap<String, String>) -> Result<SnapshotStream> {
    let bytes = serde_json::to_vec(&values)
        .map(Bytes::from)
        .map_err(|error| RuntimeError::Application(error.to_string()))?;
    Ok(Box::pin(stream::iter(
        (!bytes.is_empty()).then_some(Ok(bytes)),
    )))
}

#[cfg(test)]
mod tests {
    use futures::StreamExt;

    use super::*;

    #[tokio::test]
    async fn put_is_durable_idempotent_and_reopens() {
        let directory = tempfile::tempdir().unwrap();
        let store = KvPersistence::open(directory.path()).unwrap();
        let operation = Operation {
            lsn: 1,
            committed_lsn: 0,
            data: KvPersistence::encode_put("key".into(), "value".into()).unwrap(),
        };
        store.apply(operation.clone()).await.unwrap();
        store.apply(operation).await.unwrap();
        store.commit(1).await.unwrap();
        assert_eq!(store.get("key").as_deref(), Some("value"));
        drop(store);

        let reopened = KvPersistence::open(directory.path()).unwrap();
        assert_eq!(reopened.get("key").as_deref(), Some("value"));
        assert_eq!(reopened.durable_progress().await.unwrap().committed_lsn, 1);
    }

    #[tokio::test]
    async fn failed_persistence_does_not_publish_in_memory_durability() {
        let directory = tempfile::tempdir().unwrap();
        let store = KvPersistence::open(directory.path()).unwrap();
        let operation = Operation {
            lsn: 1,
            committed_lsn: 0,
            data: KvPersistence::encode_put("key".into(), "value".into()).unwrap(),
        };
        store.fail_next_persist.store(true, Ordering::SeqCst);
        assert!(matches!(
            store.apply(operation.clone()).await,
            Err(RuntimeError::Application(_))
        ));
        assert_eq!(store.get("key"), None);
        assert_eq!(
            store.durable_progress().await.unwrap(),
            DurableApplicationProgress::default()
        );
        assert!(!store.verify_applied(&operation).await.unwrap());

        store.apply(operation.clone()).await.unwrap();
        assert!(store.verify_applied(&operation).await.unwrap());
        drop(store);
        let reopened = KvPersistence::open(directory.path()).unwrap();
        assert_eq!(reopened.get("key").as_deref(), Some("value"));
    }

    #[tokio::test]
    async fn empty_replication_gap_returns_an_empty_stream() {
        let directory = tempfile::tempdir().unwrap();
        let store = KvPersistence::open(directory.path()).unwrap();
        let mut operations = store.get_replication_operations(2, 1).await.unwrap();
        assert!(operations.next().await.is_none());
    }

    #[tokio::test]
    async fn snapshot_at_reconstructs_the_frozen_build_boundary() {
        let directory = tempfile::tempdir().unwrap();
        let store = KvPersistence::open(directory.path()).unwrap();
        store
            .apply(Operation {
                lsn: 1,
                committed_lsn: 0,
                data: KvPersistence::encode_put("key".into(), "old".into()).unwrap(),
            })
            .await
            .unwrap();
        store
            .apply(Operation {
                lsn: 2,
                committed_lsn: 1,
                data: KvPersistence::encode_put("key".into(), "new".into()).unwrap(),
            })
            .await
            .unwrap();

        assert_eq!(
            store.snapshot_at(1).unwrap().get("key").map(String::as_str),
            Some("old")
        );
        assert_eq!(
            store.snapshot_at(2).unwrap().get("key").map(String::as_str),
            Some("new")
        );
    }

    #[tokio::test]
    async fn copied_baseline_survives_reopen_and_later_copy() {
        let first_directory = tempfile::tempdir().unwrap();
        let first = KvPersistence::open(first_directory.path()).unwrap();
        let first_build = OperationId::new("first-copy");
        first
            .apply_copy_chunk(
                &first_build,
                0,
                CopyChunk {
                    data: Bytes::from(
                        serde_json::to_vec(&BTreeMap::from([(
                            "copied".to_string(),
                            "baseline".to_string(),
                        )]))
                        .unwrap(),
                    ),
                },
            )
            .await
            .unwrap();
        first.finish_copy(&first_build, 7, 7).await.unwrap();
        first
            .apply(Operation {
                lsn: 8,
                committed_lsn: 7,
                data: KvPersistence::encode_put("later".into(), "value".into()).unwrap(),
            })
            .await
            .unwrap();
        first.commit(8).await.unwrap();

        assert_eq!(
            first.snapshot_at(7).unwrap(),
            BTreeMap::from([("copied".to_string(), "baseline".to_string())])
        );
        let expected = BTreeMap::from([
            ("copied".to_string(), "baseline".to_string()),
            ("later".to_string(), "value".to_string()),
        ]);
        assert_eq!(first.snapshot_at(8).unwrap(), expected);
        drop(first);

        let reopened = KvPersistence::open(first_directory.path()).unwrap();
        assert_eq!(reopened.snapshot_at(8).unwrap(), expected);

        let second_directory = tempfile::tempdir().unwrap();
        let second = KvPersistence::open(second_directory.path()).unwrap();
        let second_build = OperationId::new("second-copy");
        second
            .apply_copy_chunk(
                &second_build,
                0,
                CopyChunk {
                    data: Bytes::from(serde_json::to_vec(&expected).unwrap()),
                },
            )
            .await
            .unwrap();
        second.finish_copy(&second_build, 8, 8).await.unwrap();
        assert_eq!(second.snapshot_at(8).unwrap(), expected);
    }

    #[tokio::test]
    async fn incomplete_staging_chunk_is_replaced_and_not_installed() {
        let directory = tempfile::tempdir().unwrap();
        let store = KvPersistence::open(directory.path()).unwrap();
        let build = OperationId::new("interrupted-copy");
        let staging = store.copy_chunk_staging_path(&build, 0);
        std::fs::create_dir_all(staging.parent().unwrap()).unwrap();
        std::fs::write(&staging, b"partial").unwrap();

        let expected = BTreeMap::from([("key".to_string(), "value".to_string())]);
        let bytes = serde_json::to_vec(&expected).unwrap();
        store
            .apply_copy_chunk(
                &build,
                0,
                CopyChunk {
                    data: Bytes::from(bytes.clone()),
                },
            )
            .await
            .unwrap();
        assert!(!staging.exists());
        assert_eq!(
            std::fs::read(store.copy_chunk_path(&build, 0)).unwrap(),
            bytes
        );

        std::fs::write(store.copy_chunk_staging_path(&build, 1), b"ignored").unwrap();
        store.finish_copy(&build, 4, 4).await.unwrap();
        assert_eq!(store.snapshot_at(4).unwrap(), expected);
    }

    #[test]
    fn fresh_empty_proof_rejects_surviving_application_files() {
        let directory = tempfile::tempdir().unwrap();
        let application = directory.path().join("application");
        assert!(KvPersistence::is_fresh_empty(&application).unwrap());
        std::fs::create_dir_all(&application).unwrap();
        assert!(KvPersistence::is_fresh_empty(&application).unwrap());
        std::fs::write(application.join("state.json"), b"{}").unwrap();
        assert!(!KvPersistence::is_fresh_empty(&application).unwrap());
    }
}
