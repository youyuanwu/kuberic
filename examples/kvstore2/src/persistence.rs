use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Mutex;

use async_trait::async_trait;
use bytes::Bytes;
use futures::{Stream, stream};
use kuberic_protocol::types::{Epoch, OperationId};
use kuberic_runtime::application::{
    CopyChunk, DurableApplicationAck, DurableApplicationProgress, Operation,
};
use kuberic_runtime::engine::{DurableState, RetainedOperationStream};
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

pub struct KvPersistence {
    root: PathBuf,
    state_path: PathBuf,
    state: Mutex<PersistedState>,
}

impl KvPersistence {
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
        let state = if state_path.is_file() {
            serde_json::from_slice(
                &std::fs::read(&state_path)
                    .map_err(|error| RuntimeError::Application(error.to_string()))?,
            )
            .map_err(|error| RuntimeError::Application(error.to_string()))?
        } else {
            PersistedState::default()
        };
        Ok(Self {
            root,
            state_path,
            state: Mutex::new(state),
        })
    }

    pub fn get(&self, key: &str) -> Option<String> {
        self.state.lock().unwrap().values.get(key).cloned()
    }

    pub fn snapshot(&self) -> BTreeMap<String, String> {
        self.state.lock().unwrap().values.clone()
    }

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
        state.epoch = epoch;
        self.persist(&state)
    }

    fn persist(&self, state: &PersistedState) -> Result<()> {
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
}

#[async_trait]
impl DurableState for KvPersistence {
    async fn get_replication_operations(
        &self,
        from_lsn: i64,
        to_lsn: i64,
    ) -> Result<RetainedOperationStream> {
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
        let parent = path
            .parent()
            .ok_or_else(|| RuntimeError::Application("copy chunk path has no parent".into()))?;
        std::fs::create_dir_all(parent)
            .map_err(|error| RuntimeError::Application(error.to_string()))?;
        if path.is_file() {
            return if std::fs::read(&path)
                .map_err(|error| RuntimeError::Application(error.to_string()))?
                == chunk.data
            {
                Ok(())
            } else {
                Err(RuntimeError::AuthorityMismatch(
                    "copy sequence was reused with different bytes".into(),
                ))
            };
        }
        std::fs::write(path, chunk.data)
            .map_err(|error| RuntimeError::Application(error.to_string()))
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
        state.values = values;
        state.operations.clear();
        state.applied_lsn = up_to_lsn;
        state.committed_lsn = committed_lsn;
        self.persist(&state)?;
        Ok(DurableApplicationProgress {
            applied_lsn: up_to_lsn,
            committed_lsn,
        })
    }

    async fn apply(&self, operation: Operation) -> Result<DurableApplicationAck> {
        let mutation: Mutation = serde_json::from_slice(&operation.data)
            .map_err(|error| RuntimeError::Application(error.to_string()))?;
        let mut state = self.state.lock().unwrap();
        if let Some(existing) = state.operations.get(&operation.lsn) {
            if existing.data == operation.data && existing.committed_lsn == operation.committed_lsn
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
        match mutation {
            Mutation::Put { key, value } => {
                state.values.insert(key, value);
            }
        }
        state.operations.insert(
            operation.lsn,
            OperationRecord {
                committed_lsn: operation.committed_lsn,
                data: operation.data.to_vec(),
            },
        );
        state.applied_lsn = state.applied_lsn.max(operation.lsn);
        state.committed_lsn = state.committed_lsn.max(operation.committed_lsn);
        self.persist(&state)?;
        Ok(DurableApplicationProgress {
            applied_lsn: state.applied_lsn,
            committed_lsn: state.committed_lsn,
        })
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
        let mut state = self.state.lock().unwrap();
        if committed_lsn > state.applied_lsn {
            return Err(RuntimeError::Application(
                "cannot commit beyond applied progress".into(),
            ));
        }
        state.committed_lsn = state.committed_lsn.max(committed_lsn);
        self.persist(&state)?;
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
