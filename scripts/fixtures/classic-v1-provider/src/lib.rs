//! Independently compiled persisted provider and service. This crate deliberately
//! uses no runtime testing, engine storage, receipts, or additional provider hook.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{
    Arc, Mutex, OnceLock,
    atomic::{AtomicBool, Ordering},
};

use async_trait::async_trait;
use bytes::Bytes;
use futures::{StreamExt, stream};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::{Mutex as AsyncMutex, Notify, oneshot, watch};
use tokio::task::JoinSet;

use kuberic_runtime::application::{OpenContext, OperationDataStream, RoleChange};
use kuberic_runtime::protocol::types::{AccessStatus, Epoch, FaultType, ReplicaRole};
use kuberic_runtime::replicator::{
    DefaultReplicatorFactory, Replicator, ReplicatorSettings, StateReplicator,
    StatefulServicePartition,
};
use kuberic_runtime::{Result, RuntimeError, StateProvider, StatefulServiceReplica};

fn failure(error: impl std::fmt::Display) -> RuntimeError {
    RuntimeError::Application(error.to_string())
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct State {
    format: String,
    epoch: Epoch,
    committed: Vec<Bytes>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Checked {
    state: State,
    checksum: String,
}

fn checksum(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn encode(state: &State) -> Result<Vec<u8>> {
    let payload = serde_json::to_vec(state).map_err(failure)?;
    serde_json::to_vec(&Checked {
        state: state.clone(),
        checksum: checksum(&payload),
    })
    .map_err(failure)
}

fn decode(bytes: &[u8]) -> Result<State> {
    let checked: Checked = serde_json::from_slice(bytes).map_err(failure)?;
    let payload = serde_json::to_vec(&checked.state).map_err(failure)?;
    if checked.state.format != "classic-external-v1"
        || checked.checksum != checksum(&payload)
        || checked.state.epoch.data_loss_number < 0
        || checked.state.epoch.configuration_number < 0
        || checked.state.committed.len() >= i64::MAX as usize
    {
        return Err(failure("incompatible or corrupt external provider state"));
    }
    Ok(checked.state)
}

struct Store {
    root: PathBuf,
    state: Mutex<State>,
    changed: Notify,
}

impl Store {
    fn open(root: &Path) -> Result<Self> {
        std::fs::create_dir_all(root).map_err(failure)?;
        let state = match std::fs::read(root.join("state.json")) {
            Ok(bytes) => decode(&bytes)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if std::fs::read_dir(root)
                    .map_err(failure)?
                    .next()
                    .transpose()
                    .map_err(failure)?
                    .is_some()
                {
                    return Err(failure(
                        "established external storage lacks metadata; refusing empty recovery",
                    ));
                }
                State {
                    format: "classic-external-v1".into(),
                    epoch: Epoch::default(),
                    committed: Vec::new(),
                }
            }
            Err(error) => return Err(failure(error)),
        };
        Ok(Self {
            root: root.to_path_buf(),
            state: Mutex::new(state),
            changed: Notify::new(),
        })
    }

    fn persist(&self, state: &State) -> Result<()> {
        let bytes = encode(state)?;
        let path = self.root.join("state.json");
        let temporary = self.root.join("state.tmp");
        let mut file = std::fs::File::create(&temporary).map_err(failure)?;
        file.write_all(&bytes).map_err(failure)?;
        file.sync_all().map_err(failure)?;
        std::fs::rename(temporary, path).map_err(failure)?;
        std::fs::File::open(&self.root)
            .map_err(failure)?
            .sync_all()
            .map_err(failure)
    }

    fn commit(&self, lsn: i64, data: Bytes) -> Result<()> {
        let mut state = self.state.lock().unwrap();
        if lsn <= 0 {
            return Err(failure("invalid service sequence"));
        }
        if lsn <= state.committed.len() as i64 {
            return if state.committed.get((lsn - 1) as usize) == Some(&data) {
                Ok(())
            } else {
                Err(failure("conflicting duplicate service data"))
            };
        }
        if lsn != state.committed.len() as i64 + 1 {
            return Err(failure("service sequence gap"));
        }
        let mut next = state.clone();
        next.committed.push(data);
        self.persist(&next)?;
        *state = next;
        self.changed.notify_waiters();
        Ok(())
    }

    fn install(&self, data: &Bytes) -> Result<()> {
        let next = decode(data)?;
        let mut state = self.state.lock().unwrap();
        if !next.committed.starts_with(&state.committed) {
            return Err(failure("copy conflicts with acknowledged service prefix"));
        }
        self.persist(&next)?;
        *state = next;
        self.changed.notify_waiters();
        Ok(())
    }
}

struct Provider {
    root: PathBuf,
    store: OnceLock<Arc<Store>>,
    aborted: Arc<AtomicBool>,
    recovery: Mutex<Option<std::result::Result<Vec<Bytes>, String>>>,
    hold_recovery: AtomicBool,
    recovery_entered: Notify,
    recovery_release: Notify,
    advertised_context: Vec<Bytes>,
    expected_context: Vec<Bytes>,
    context_observations: Mutex<CopyContextObservations>,
}

/// Observations recorded by actual copy-context and copy-state callbacks.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CopyContextObservations {
    pub context_acquisitions: usize,
    pub copy_state_acquisitions: Vec<CopyStateObservation>,
}

/// One copy-state callback's boundary, received items, and observed termination.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CopyStateObservation {
    pub up_to: i64,
    pub items: Vec<Bytes>,
    pub reached_eof: bool,
}

/// Committed application state decoded from the provider's checked file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PersistedSnapshot {
    pub committed_lsn: i64,
    pub values: Vec<Bytes>,
}

impl Provider {
    fn store(&self) -> Result<&Arc<Store>> {
        self.store.get().ok_or(RuntimeError::NotOpen)
    }
}

#[async_trait]
impl StateProvider for Provider {
    async fn update_epoch(&self, epoch: Epoch, _: i64) -> Result<()> {
        let store = self.store()?;
        let mut state = store.state.lock().unwrap();
        if epoch < state.epoch {
            return Err(failure("service epoch regressed"));
        }
        let mut next = state.clone();
        next.epoch = epoch;
        store.persist(&next)?;
        *state = next;
        Ok(())
    }
    async fn last_committed_lsn(&self) -> Result<i64> {
        Ok(self.store()?.state.lock().unwrap().committed.len() as i64)
    }
    async fn get_copy_context(&self) -> Result<OperationDataStream> {
        self.context_observations
            .lock()
            .unwrap()
            .context_acquisitions += 1;
        Ok(Box::pin(stream::iter(
            self.advertised_context.clone().into_iter().map(Ok),
        )))
    }
    async fn get_copy_state(
        &self,
        up_to: i64,
        mut context: OperationDataStream,
    ) -> Result<OperationDataStream> {
        let acquisition = {
            let mut observations = self.context_observations.lock().unwrap();
            let index = observations.copy_state_acquisitions.len();
            observations
                .copy_state_acquisitions
                .push(CopyStateObservation {
                    up_to,
                    items: Vec::new(),
                    reached_eof: false,
                });
            index
        };
        let mut received = 0;
        while let Some(item) = context.next().await {
            let item = item?;
            self.context_observations
                .lock()
                .unwrap()
                .copy_state_acquisitions[acquisition]
                .items
                .push(item.clone());
            if self.expected_context.get(received) != Some(&item) {
                return Err(failure("unexpected copy context"));
            }
            received += 1;
        }
        self.context_observations
            .lock()
            .unwrap()
            .copy_state_acquisitions[acquisition]
            .reached_eof = true;
        if received != self.expected_context.len() {
            return Err(failure("incomplete copy context"));
        }
        if up_to < 0 {
            return Err(failure("negative copy boundary"));
        }
        let store = self.store()?;
        loop {
            let changed = store.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if self.aborted.load(Ordering::Acquire) {
                return Err(RuntimeError::Closed);
            }
            let snapshot = {
                let state = store.state.lock().unwrap();
                if state.committed.len() as i64 >= up_to {
                    let mut snapshot = state.clone();
                    snapshot.committed.truncate(up_to as usize);
                    Some(Bytes::from(encode(&snapshot)?))
                } else {
                    None
                }
            };
            if let Some(snapshot) = snapshot {
                return Ok(Box::pin(stream::iter([Ok(snapshot)])));
            }
            changed.await;
        }
    }
    async fn on_data_loss(&self) -> Result<bool> {
        self.recovery_entered.notify_one();
        if self.hold_recovery.load(Ordering::Acquire) {
            self.recovery_release.notified().await;
        }
        let recovery = self.recovery.lock().unwrap().take();
        match recovery {
            None => Ok(false),
            Some(Err(error)) => Err(failure(error)),
            Some(Ok(values)) => {
                let store = self.store()?;
                let mut state = store.state.lock().unwrap();
                let mut next = state.clone();
                next.committed = values;
                store.persist(&next)?;
                *state = next;
                store.changed.notify_waiters();
                Ok(true)
            }
        }
    }
}

pub struct PendingWrite {
    pub lsn: i64,
    outcome: oneshot::Receiver<Result<i64>>,
}
impl PendingWrite {
    pub async fn wait(self) -> Result<i64> {
        self.outcome.await.map_err(failure)?
    }
}

#[derive(Default)]
struct Tasks {
    tasks: JoinSet<Result<()>>,
    failures: Vec<String>,
}
impl Tasks {
    async fn drain(&mut self) -> Result<()> {
        while let Some(result) = self.tasks.join_next().await {
            match result {
                Ok(Ok(())) => {}
                Ok(Err(error)) => self.failures.push(error.to_string()),
                Err(error) => self.failures.push(error.to_string()),
            }
        }
        if self.failures.is_empty() {
            Ok(())
        } else {
            Err(failure(self.failures.join("; ")))
        }
    }
}

pub struct ExternalService {
    provider: Arc<Provider>,
    address: String,
    partition: Mutex<Option<StatefulServicePartition>>,
    control: Mutex<Option<Arc<dyn Replicator>>>,
    replicator: Mutex<Option<Arc<dyn StateReplicator>>>,
    write_gate: Arc<AsyncMutex<()>>,
    writes: AsyncMutex<Tasks>,
    streams: AsyncMutex<Tasks>,
    stop: watch::Sender<bool>,
    closed: Arc<AtomicBool>,
    copying: Arc<AtomicBool>,
    copy_done: Arc<Notify>,
    role: Mutex<ReplicaRole>,
    aborted: Arc<AtomicBool>,
}

impl ExternalService {
    pub fn new(root: PathBuf, address: String) -> Self {
        Self::with_copy_context(root, address, Vec::new(), Vec::new())
    }

    /// Configures independently advertised context and the accepted incoming sequence.
    ///
    /// Expected items validate the supplied stream; they never replace its input.
    pub fn with_copy_context(
        root: PathBuf,
        address: String,
        advertised_context: Vec<Bytes>,
        expected_context: Vec<Bytes>,
    ) -> Self {
        let aborted = Arc::new(AtomicBool::new(false));
        Self {
            provider: Arc::new(Provider {
                root,
                store: OnceLock::new(),
                aborted: aborted.clone(),
                recovery: Mutex::new(None),
                hold_recovery: AtomicBool::new(false),
                recovery_entered: Notify::new(),
                recovery_release: Notify::new(),
                advertised_context,
                expected_context,
                context_observations: Mutex::new(CopyContextObservations::default()),
            }),
            address,
            partition: Mutex::new(None),
            control: Mutex::new(None),
            replicator: Mutex::new(None),
            write_gate: Arc::new(AsyncMutex::new(())),
            writes: AsyncMutex::new(Tasks::default()),
            streams: AsyncMutex::new(Tasks::default()),
            stop: watch::channel(false).0,
            closed: Arc::new(AtomicBool::new(false)),
            copying: Arc::new(AtomicBool::new(false)),
            copy_done: Arc::new(Notify::new()),
            role: Mutex::new(ReplicaRole::None),
            aborted,
        }
    }

    /// Returns an owned snapshot of observations recorded by provider callbacks.
    pub fn copy_context_observations(&self) -> CopyContextObservations {
        self.provider.context_observations.lock().unwrap().clone()
    }

    /// Reads persisted application state with the normal format/checksum decoder.
    ///
    /// This does not grant serving access or relax [`Self::values`]'s access check.
    pub fn read_persisted_snapshot(root: &Path) -> Result<PersistedSnapshot> {
        let state = decode(&std::fs::read(root.join("state.json")).map_err(failure)?)?;
        Ok(PersistedSnapshot {
            committed_lsn: state.committed.len() as i64,
            values: state.committed,
        })
    }

    pub fn is_fresh(root: &Path) -> std::io::Result<bool> {
        match std::fs::read_dir(root) {
            Ok(mut entries) => Ok(entries.next().transpose()?.is_none()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(true),
            Err(error) => Err(error),
        }
    }

    pub fn pause_recovery(&self, restored: Option<std::result::Result<Vec<Bytes>, String>>) {
        *self.provider.recovery.lock().unwrap() = restored;
        self.provider.hold_recovery.store(true, Ordering::Release);
    }

    pub async fn recovery_entered(&self) {
        self.provider.recovery_entered.notified().await;
    }

    pub fn release_recovery(&self) {
        self.provider.recovery_release.notify_one();
    }
    pub async fn begin_write(&self, data: Bytes) -> Result<PendingWrite> {
        let permit = self.write_gate.clone().lock_owned().await;
        let mut writes = self.writes.lock().await;
        if self.closed.load(Ordering::Acquire) {
            return Err(RuntimeError::Closed);
        }
        let replicator = self
            .replicator
            .lock()
            .unwrap()
            .clone()
            .ok_or(RuntimeError::NotOpen)?;
        let store = self.provider.store()?.clone();
        let closed = self.closed.clone();
        let aborted = self.aborted.clone();
        let mut stop = self.stop.subscribe();
        let partition = self
            .partition
            .lock()
            .unwrap()
            .clone()
            .ok_or(RuntimeError::NotOpen)?;
        let (reply, outcome) = oneshot::channel();
        let assigned = replicator.begin_replicate(data.clone())?;
        let lsn = assigned.lsn;
        writes.tasks.spawn(async move {
            let _permit = permit;
            let replicated = tokio::select! {
                biased;
                _ = async { let _ = stop.wait_for(|stopped| *stopped).await; } => Err(failure("external service aborted")),
                result = assigned.completion => result,
            };
            let result = replicated.and_then(|()| {
                if *stop.borrow() { return Err(failure("external service aborted")); }
                store.commit(lsn, data)?;
                Ok(lsn)
            }).map_err(|error| RuntimeError::AdmittedOperationFailed { lsn, reason: error.to_string() });
            let error = result.as_ref().err().map(ToString::to_string);
            let _ = reply.send(result);
            if let Some(error) = error {
                let was_closed = closed.swap(true, Ordering::AcqRel);
                aborted.store(true, Ordering::Release);
                store.changed.notify_waiters();
                if !was_closed { partition.report_fault(FaultType::Transient).await?; }
                return Err(failure(error));
            }
            Ok(())
        });
        Ok(PendingWrite { lsn, outcome })
    }
    pub async fn values(&self) -> Result<Vec<Bytes>> {
        if self.closed.load(Ordering::Acquire) {
            return Err(RuntimeError::Closed);
        }
        let partition = self
            .partition
            .lock()
            .unwrap()
            .clone()
            .ok_or(RuntimeError::NotOpen)?;
        let access = partition.get_read_status().await?;
        if access != AccessStatus::Granted {
            return Err(RuntimeError::ReadClosed(access));
        }
        Ok(self
            .provider
            .store()?
            .state
            .lock()
            .unwrap()
            .committed
            .clone())
    }
    pub async fn shutdown_and_drain(&self) -> Result<()> {
        self.close().await
    }

    async fn secondary(&self, copy: bool) -> Result<()> {
        let replicator = self
            .replicator
            .lock()
            .unwrap()
            .clone()
            .ok_or(RuntimeError::NotOpen)?;
        let store = self.provider.store()?.clone();
        let partition = self
            .partition
            .lock()
            .unwrap()
            .clone()
            .ok_or(RuntimeError::NotOpen)?;
        let mut tasks = self.streams.lock().await;
        let (copied, mut finished) = watch::channel(!copy);
        if copy {
            self.copying.store(true, Ordering::Release);
            let copying = self.copying.clone();
            let copy_done = self.copy_done.clone();
            let mut stream = replicator.copy_operations()?;
            let store = store.clone();
            let partition = partition.clone();
            let mut stop = self.stop.subscribe();
            tasks.tasks.spawn(async move {
                let receive = async move {
                    let mut installed = false;
                    let mut eof = false;
                    while let Some(operation) = stream.get_operation().await? {
                        if !operation.end_of_stream {
                            if let Err(error) = store.install(&operation.data) {
                                stream.report_fault(error.to_string())?;
                                partition.report_fault(FaultType::Transient).await?;
                                return Err(error);
                            }
                            installed = true;
                        } else {
                            eof = true;
                        }
                        operation.acknowledge()?;
                    }
                    if eof {
                        if !installed {
                            return Err(failure("copy EOF without service snapshot"));
                        }
                        copying.store(false, Ordering::Release);
                        copy_done.notify_waiters();
                        copied.send_replace(true);
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
        tasks.tasks.spawn(async move {
            let receive = async move {
                if !*finished.borrow() && finished.wait_for(|copied| *copied).await.is_err() {
                    return Ok(());
                }
                while let Some(operation) = stream.get_operation().await? {
                    if !operation.end_of_stream
                        && let Err(error) = store.commit(operation.lsn, operation.data.clone())
                    {
                        stream.report_fault(error.to_string())?;
                        partition.report_fault(FaultType::Transient).await?;
                        return Err(error);
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
}

#[async_trait]
impl StatefulServiceReplica for ExternalService {
    async fn open(self: Arc<Self>, context: OpenContext) -> Result<Arc<dyn Replicator>> {
        if self.closed.load(Ordering::Acquire) {
            return Err(RuntimeError::Closed);
        }
        if let Some(control) = self.control.lock().unwrap().clone() {
            return Ok(control);
        }
        if self.provider.store.get().is_none() {
            if context.mode == kuberic_runtime::application::OpenMode::Existing
                && !self.provider.root.join("state.json").is_file()
            {
                return Err(failure("established external provider is missing metadata"));
            }
            let store = Arc::new(Store::open(&self.provider.root)?);
            store.persist(&store.state.lock().unwrap())?;
            self.provider
                .store
                .set(store)
                .map_err(|_| failure("provider opened concurrently"))?;
        }
        *self.partition.lock().unwrap() = Some(context.partition.clone());
        let interfaces = context
            .partition
            .with_factory(Arc::new(DefaultReplicatorFactory::new()))
            .create_replicator(
                Some(self.provider.clone()),
                Some(ReplicatorSettings {
                    replication_address: self.address.clone(),
                }),
            )
            .await?;
        let control = interfaces.replicator();
        if self.closed.load(Ordering::Acquire) {
            control.abort();
            return Err(RuntimeError::Closed);
        }
        *self.replicator.lock().unwrap() = interfaces.state_replicator();
        *self.control.lock().unwrap() = Some(control.clone());
        Ok(control)
    }
    async fn change_role(&self, role: ReplicaRole) -> Result<RoleChange> {
        let previous = *self.role.lock().unwrap();
        if role == ReplicaRole::Primary || previous == ReplicaRole::Primary {
            self.writes.lock().await.drain().await?;
            while self.copying.load(Ordering::Acquire) {
                let changed = self.copy_done.notified();
                tokio::pin!(changed);
                changed.as_mut().enable();
                if !self.copying.load(Ordering::Acquire) {
                    break;
                }
                if self.closed.load(Ordering::Acquire) {
                    return Err(RuntimeError::Closed);
                }
                changed.await;
            }
        }
        match role {
            ReplicaRole::IdleSecondary => self.secondary(true).await?,
            ReplicaRole::ActiveSecondary if previous == ReplicaRole::Primary => {
                self.secondary(false).await?
            }
            ReplicaRole::ActiveSecondary if previous == ReplicaRole::None => {
                self.secondary(true).await?
            }
            _ => {}
        }
        *self.role.lock().unwrap() = role;
        Ok(RoleChange {
            service_address: None,
        })
    }
    async fn close(&self) -> Result<()> {
        self.closed.store(true, Ordering::Release);
        let writes = self.writes.lock().await.drain().await;
        self.stop.send_replace(true);
        let streams = self.streams.lock().await.drain().await;
        self.partition.lock().unwrap().take();
        self.replicator.lock().unwrap().take();
        self.control.lock().unwrap().take();
        writes.and(streams)
    }
    fn abort(&self) {
        self.closed.store(true, Ordering::Release);
        self.aborted.store(true, Ordering::Release);
        self.stop.send_replace(true);
        self.copy_done.notify_waiters();
        if let Some(store) = self.provider.store.get() {
            store.changed.notify_waiters();
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn target_context() -> Vec<Bytes> {
        vec![
            Bytes::from_static(b"target/context"),
            Bytes::new(),
            Bytes::from_static(b"target/trailer"),
        ]
    }

    fn input(items: Vec<Bytes>) -> OperationDataStream {
        Box::pin(stream::iter(items.into_iter().map(Ok)))
    }

    async fn rejects(provider: &Provider, context: OperationDataStream, expected: &str) {
        let result =
            tokio::time::timeout(Duration::from_secs(1), provider.get_copy_state(1, context))
                .await
                .expect("invalid context must reject without waiting for the remaining stream");
        match result {
            Err(RuntimeError::Application(message)) => assert_eq!(message, expected),
            Err(other) => panic!("wrong rejection: {other}"),
            Ok(_) => panic!("invalid context exposed a successful snapshot stream"),
        }
    }

    async fn succeeds_only_after_eof(provider: &Provider, items: Vec<Bytes>) {
        let (release, eof) = oneshot::channel();
        let tail = stream::once(async move {
            eof.await.unwrap();
            None::<Result<Bytes>>
        })
        .filter_map(futures::future::ready);
        let copy = provider.get_copy_state(1, Box::pin(input(items).chain(tail)));
        tokio::pin!(copy);
        assert!(
            tokio::time::timeout(Duration::from_millis(25), &mut copy)
                .await
                .is_err(),
            "matching items without EOF must not expose a snapshot"
        );
        assert!(
            !provider
                .context_observations
                .lock()
                .unwrap()
                .copy_state_acquisitions
                .last()
                .unwrap()
                .reached_eof
        );
        release.send(()).unwrap();
        let mut snapshot = tokio::time::timeout(Duration::from_secs(1), &mut copy)
            .await
            .expect("EOF releases snapshot acquisition")
            .unwrap();
        let copied = decode(&snapshot.next().await.unwrap().unwrap()).unwrap();
        assert_eq!(copied.committed, [Bytes::from_static(b"application/value")]);
        assert!(snapshot.next().await.is_none());
        assert!(
            provider
                .context_observations
                .lock()
                .unwrap()
                .copy_state_acquisitions
                .last()
                .unwrap()
                .reached_eof
        );
    }

    fn opened_service(root: &Path, expected: Vec<Bytes>) -> ExternalService {
        let service = ExternalService::with_copy_context(
            root.to_path_buf(),
            "http://127.0.0.1:0".into(),
            vec![
                Bytes::from_static(b"source/context"),
                Bytes::new(),
                Bytes::from_static(b"source/trailer"),
            ],
            expected,
        );
        let store = Arc::new(Store::open(root).unwrap());
        store
            .commit(1, Bytes::from_static(b"application/value"))
            .unwrap();
        assert!(service.provider.store.set(store).is_ok());
        service
    }

    #[tokio::test]
    async fn copy_context_policy_rejects_non_target_or_incomplete_streams() {
        let root = tempfile::tempdir().unwrap();
        let expected = target_context();
        let service = opened_service(root.path(), expected.clone());
        let provider = &service.provider;
        let mut surplus = expected.clone();
        surplus.push(Bytes::from_static(b"surplus"));
        let source = provider.advertised_context.clone();
        let no_middle = vec![expected[0].clone(), expected[2].clone()];
        let changed = vec![
            expected[0].clone(),
            Bytes::from_static(b"changed"),
            expected[2].clone(),
        ];
        let reordered = vec![
            expected[2].clone(),
            expected[1].clone(),
            expected[0].clone(),
        ];
        for items in [surplus, source, no_middle, changed, reordered] {
            rejects(provider, input(items), "unexpected copy context").await;
        }
        for items in [Vec::new(), expected[..1].to_vec(), expected[..2].to_vec()] {
            rejects(provider, input(items), "incomplete copy context").await;
        }
        rejects(
            provider,
            Box::pin(input(vec![Bytes::from_static(b"source/context")]).chain(stream::pending())),
            "unexpected copy context",
        )
        .await;
        rejects(
            provider,
            Box::pin(input(expected.clone()).chain(stream::iter([Err(failure(
                "injected context stream failure",
            ))]))),
            "injected context stream failure",
        )
        .await;
        succeeds_only_after_eof(provider, expected.clone()).await;
        let observed = service.copy_context_observations();
        assert_eq!(observed.context_acquisitions, 0);
        assert_eq!(
            observed.copy_state_acquisitions.last().unwrap().items,
            expected
        );
        assert!(
            observed
                .copy_state_acquisitions
                .iter()
                .all(|call| call.up_to == 1)
        );
    }

    #[tokio::test]
    async fn default_copy_context_policy_still_requires_empty_stream() {
        let root = tempfile::tempdir().unwrap();
        let service = ExternalService::new(root.path().to_path_buf(), "http://127.0.0.1:0".into());
        let store = Arc::new(Store::open(root.path()).unwrap());
        store
            .commit(1, Bytes::from_static(b"application/value"))
            .unwrap();
        assert!(service.provider.store.set(store).is_ok());
        let provider = &service.provider;
        let mut advertised = provider.get_copy_context().await.unwrap();
        assert!(advertised.next().await.is_none());
        rejects(
            provider,
            input(vec![Bytes::new()]),
            "unexpected copy context",
        )
        .await;
        rejects(
            provider,
            Box::pin(input(vec![Bytes::new()]).chain(stream::pending())),
            "unexpected copy context",
        )
        .await;
        rejects(
            provider,
            Box::pin(stream::iter([Err(failure(
                "injected context stream failure",
            ))])),
            "injected context stream failure",
        )
        .await;
        succeeds_only_after_eof(provider, Vec::new()).await;
        let observed = service.copy_context_observations();
        assert_eq!(observed.context_acquisitions, 1);
        assert_eq!(observed.copy_state_acquisitions[1].items, [Bytes::new()]);
        assert!(!observed.copy_state_acquisitions[1].reached_eof);
    }
}
