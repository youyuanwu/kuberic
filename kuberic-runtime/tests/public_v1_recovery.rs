#![cfg(feature = "testing")]

// These journeys exercise native receiver lifetimes and service-owned durable
// state. Transport replies and native progress are deliberately not commit receipts.

#[path = "support/read_fencing.rs"]
mod read_fencing;
#[path = "public_v1_recovery/rejection_order.rs"]
mod rejection_order;
#[path = "support/staged_access.rs"]
mod staged_access;

use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use futures::{Stream, StreamExt, stream};
use kuberic_controller::cluster_api::PublicV1Scope;
use kuberic_controller::evaluator::testing::*;
#[allow(
    clippy::disallowed_types,
    reason = "observes protected callbacks on the same native creation"
)]
use kuberic_runtime::PrimaryReplicator;
use kuberic_runtime::application::{OpenContext, OperationDataStream, RoleChange};
use kuberic_runtime::host::{
    ApplicationStorageState, ReplicaHandle, ReplicaHost, ReplicaProcessConfig, RunningReplica,
};
use kuberic_runtime::protocol::command::{
    ProtocolCommand, PublicV1HistoryCommand, PublicV1SessionRenewalCommand,
};
use kuberic_runtime::protocol::public_operations::*;
use kuberic_runtime::protocol::types::*;
use kuberic_runtime::replicator::stream::{OperationKind, OperationStream, ReceivedOperation};
use kuberic_runtime::replicator::{
    DefaultReplicatorFactory, ReplicaInformation, ReplicaSetConfiguration, ReplicaSetQuorumMode,
    Replicator, ReplicatorFactory, ReplicatorFactoryContext, ReplicatorInterfaces,
    ReplicatorSettings, StateReplicator, StatefulServicePartition, TestingTcpRoute,
    TestingTcpRouteKind,
};
use kuberic_runtime::testing::sqlite_store::SqliteStore;
use kuberic_runtime::testing::state::{SCHEMA_VERSION, StorageIdentity};
use kuberic_runtime::{Result, RuntimeError, StateProvider, StatefulServiceReplica};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::{Mutex as AsyncMutex, Notify, RwLock, oneshot, watch};
use tonic::Request;

#[allow(dead_code, reason = "independently compiled native carrier fixture")]
mod native_wire {
    tonic::include_proto!("kuberic.sf.v1");
}

const TOKEN: &str = "preview-token";
const EPOCH: Epoch = Epoch::new(0, 1);
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

fn failure(error: impl std::fmt::Display) -> RuntimeError {
    RuntimeError::Application(error.to_string())
}

fn root() -> tempfile::TempDir {
    let directory = std::env::var_os("TMPDIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../target/paw-tmp"));
    std::fs::create_dir_all(&directory).unwrap();
    tempfile::Builder::new()
        .prefix("native-recovery-")
        .tempdir_in(directory)
        .unwrap()
}

#[derive(Default)]
struct Gate {
    held: AtomicBool,
    entered: Notify,
    released: Notify,
}

impl Gate {
    fn hold(&self) {
        self.held.store(true, Ordering::Release);
    }

    fn release(&self) {
        self.held.store(false, Ordering::Release);
        self.released.notify_waiters();
    }

    async fn wait(&self) {
        if self.held.load(Ordering::Acquire) {
            self.entered.notify_one();
        }
        loop {
            let released = self.released.notified();
            tokio::pin!(released);
            released.as_mut().enable();
            if !self.held.load(Ordering::Acquire) {
                return;
            }
            released.await;
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ServiceState {
    format: String,
    epoch: Epoch,
    values: Vec<Bytes>,
}

impl Default for ServiceState {
    fn default() -> Self {
        Self {
            format: "native-recovery-service-v1".into(),
            epoch: Epoch::default(),
            values: Vec::new(),
        }
    }
}

impl ServiceState {
    fn decode(bytes: &[u8]) -> Result<Self> {
        let state: Self = serde_json::from_slice(bytes).map_err(failure)?;
        if state.format != Self::default().format
            || state.epoch.data_loss_number < 0
            || state.epoch.configuration_number < 0
        {
            return Err(failure("incompatible service snapshot"));
        }
        Ok(state)
    }
}

struct Provider {
    root: PathBuf,
    state: Mutex<ServiceState>,
    identity: Mutex<Option<ReplicaIdentity>>,
    copy: Gate,
    install: Gate,
    commit: Gate,
    recovery: Gate,
    changed: Notify,
    fail_commit: AtomicBool,
    copy_failure: AtomicU64,
    catchup_boundary: AtomicU64,
    catchup_entered: Notify,
    catchup_probe: Notify,
    corrupt_copy: AtomicBool,
    recovery_outcome: AtomicU64,
    recovery_calls: AtomicU64,
    epoch_tails: Mutex<Vec<i64>>,
    synchronous_install: Mutex<Option<std::sync::mpsc::Receiver<()>>>,
    synchronous_entered: Notify,
    installs: AtomicU64,
    copy_active: AtomicU64,
    copy_interrupted: AtomicU64,
    copy_context_calls: AtomicU64,
    copy_calls: AtomicU64,
    progress_calls: AtomicU64,
    query_current: AtomicU64,
    query_capability: AtomicU64,
    conservative_quorum: AtomicBool,
    probe_fenced_admission: AtomicBool,
    close_failure: AtomicBool,
    synchronous_close: Mutex<Option<std::sync::mpsc::Receiver<()>>>,
    close_entered: Notify,
    synchronous_epoch: Mutex<Option<std::sync::mpsc::Receiver<()>>>,
    epoch_entered: Notify,
}

impl Provider {
    fn new(root: PathBuf) -> Result<Self> {
        let state = match std::fs::read(root.join("service.json")) {
            Ok(bytes) => ServiceState::decode(&bytes)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if root.exists() && std::fs::read_dir(&root).map_err(failure)?.next().is_some() {
                    return Err(failure("established service metadata is missing"));
                }
                ServiceState::default()
            }
            Err(error) => return Err(failure(error)),
        };
        Ok(Self {
            root,
            state: Mutex::new(state),
            identity: Mutex::new(None),
            copy: Gate::default(),
            install: Gate::default(),
            commit: Gate::default(),
            recovery: Gate::default(),
            changed: Notify::new(),
            fail_commit: AtomicBool::new(false),
            copy_failure: AtomicU64::new(0),
            catchup_boundary: AtomicU64::new(0),
            catchup_entered: Notify::new(),
            catchup_probe: Notify::new(),
            corrupt_copy: AtomicBool::new(false),
            recovery_outcome: AtomicU64::new(0),
            recovery_calls: AtomicU64::new(0),
            epoch_tails: Mutex::new(Vec::new()),
            synchronous_install: Mutex::new(None),
            synchronous_entered: Notify::new(),
            installs: AtomicU64::new(0),
            copy_active: AtomicU64::new(0),
            copy_interrupted: AtomicU64::new(0),
            copy_context_calls: AtomicU64::new(0),
            copy_calls: AtomicU64::new(0),
            progress_calls: AtomicU64::new(0),
            query_current: AtomicU64::new(0),
            query_capability: AtomicU64::new(0),
            conservative_quorum: AtomicBool::new(false),
            probe_fenced_admission: AtomicBool::new(false),
            close_failure: AtomicBool::new(false),
            synchronous_close: Mutex::new(None),
            close_entered: Notify::new(),
            synchronous_epoch: Mutex::new(None),
            epoch_entered: Notify::new(),
        })
    }

    fn persist(&self, state: &ServiceState) -> Result<()> {
        std::fs::create_dir_all(&self.root).map_err(failure)?;
        let bytes = serde_json::to_vec(state).map_err(failure)?;
        let pending = self.root.join("service.pending");
        std::fs::write(&pending, bytes).map_err(failure)?;
        std::fs::File::open(&pending)
            .and_then(|file| file.sync_all())
            .map_err(failure)?;
        std::fs::rename(pending, self.root.join("service.json")).map_err(failure)?;
        std::fs::File::open(&self.root)
            .and_then(|directory| directory.sync_all())
            .map_err(failure)
    }

    fn values(&self) -> Vec<Bytes> {
        self.state.lock().unwrap().values.clone()
    }

    fn callback_counts(&self) -> (u64, u64, u64, usize, u64) {
        (
            self.progress_calls.load(Ordering::SeqCst),
            self.copy_context_calls.load(Ordering::SeqCst),
            self.copy_calls.load(Ordering::SeqCst),
            self.epoch_tails.lock().unwrap().len(),
            self.recovery_calls.load(Ordering::SeqCst),
        )
    }

    fn commit(&self, lsn: i64, data: Bytes) -> Result<()> {
        if self.fail_commit.swap(false, Ordering::AcqRel) {
            return Err(failure("injected service persistence failure"));
        }
        let mut state = self.state.lock().unwrap();
        if lsn <= 0 {
            return Err(failure("invalid service LSN"));
        }
        if lsn <= state.values.len() as i64 {
            return if state.values.get((lsn - 1) as usize) == Some(&data) {
                Ok(())
            } else {
                Err(failure("service history conflicts at an existing LSN"))
            };
        }
        if lsn != state.values.len() as i64 + 1 {
            return Err(failure("service history has a gap"));
        }
        let mut candidate = state.clone();
        candidate.values.push(data);
        self.persist(&candidate)?;
        *state = candidate;
        self.changed.notify_waiters();
        Ok(())
    }

    async fn persist_received(&self, operation: &ReceivedOperation) -> Result<()> {
        if operation.end_of_stream {
            return Ok(());
        }
        match operation.kind {
            OperationKind::Replication => {
                self.commit.wait().await;
                self.commit(operation.lsn, operation.data.clone())
            }
            OperationKind::Copy => {
                let snapshot = ServiceState::decode(&operation.data)?;
                self.install.wait().await;
                let synchronous = self.synchronous_install.lock().unwrap().take();
                if let Some(blocked) = synchronous {
                    self.synchronous_entered.notify_one();
                    blocked
                        .recv_timeout(Duration::from_secs(15))
                        .map_err(failure)?;
                }
                let mut state = self.state.lock().unwrap();
                if !snapshot.values.starts_with(&state.values) {
                    return Err(failure("copy conflicts with committed service history"));
                }
                self.persist(&snapshot)?;
                *state = snapshot;
                self.installs.fetch_add(1, Ordering::SeqCst);
                self.changed.notify_waiters();
                Ok(())
            }
        }
    }
}

#[async_trait]
impl StateProvider for Provider {
    async fn update_epoch(&self, epoch: Epoch, previous_epoch_last_lsn: i64) -> Result<()> {
        let synchronous = self.synchronous_epoch.lock().unwrap().take();
        if let Some(blocked) = synchronous {
            self.epoch_entered.notify_one();
            blocked
                .recv_timeout(Duration::from_secs(15))
                .map_err(failure)?;
        }
        let mut state = self.state.lock().unwrap();
        if epoch < state.epoch || previous_epoch_last_lsn < 0 {
            return Err(failure("service epoch regressed"));
        }
        self.epoch_tails
            .lock()
            .unwrap()
            .push(previous_epoch_last_lsn);
        let mut next = state.clone();
        next.epoch = epoch;
        self.persist(&next)?;
        *state = next;
        Ok(())
    }

    async fn last_committed_lsn(&self) -> Result<i64> {
        self.progress_calls.fetch_add(1, Ordering::SeqCst);
        Ok(self.state.lock().unwrap().values.len() as i64)
    }

    async fn get_copy_context(&self) -> Result<OperationDataStream> {
        self.copy_context_calls.fetch_add(1, Ordering::SeqCst);
        Ok(Box::pin(stream::empty()))
    }

    async fn get_copy_state(
        &self,
        up_to: i64,
        mut context: OperationDataStream,
    ) -> Result<OperationDataStream> {
        self.copy_calls.fetch_add(1, Ordering::SeqCst);
        match self.copy_failure.load(Ordering::Acquire) {
            1 => return Err(failure("injected source GetCopyState failure")),
            2 => {
                return Ok(Box::pin(stream::iter([Err(failure(
                    "injected source EndGetNext failure",
                ))])));
            }
            _ => {}
        }
        struct Activity<'a> {
            active: &'a AtomicU64,
            interrupted: &'a AtomicU64,
            completed: bool,
        }
        impl Drop for Activity<'_> {
            fn drop(&mut self) {
                self.active.fetch_sub(1, Ordering::SeqCst);
                if !self.completed {
                    self.interrupted.fetch_add(1, Ordering::SeqCst);
                }
            }
        }
        self.copy_active.fetch_add(1, Ordering::SeqCst);
        let mut activity = Activity {
            active: &self.copy_active,
            interrupted: &self.copy_interrupted,
            completed: false,
        };
        if let Some(item) = context.next().await {
            item?;
            return Err(failure("unexpected service copy context"));
        }
        let snapshot = loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            let snapshot = {
                let state = self.state.lock().unwrap();
                if up_to < 0 {
                    return Err(failure("negative service snapshot boundary"));
                }
                (state.values.len() as i64 >= up_to).then(|| {
                    let mut snapshot = state.clone();
                    snapshot.values.truncate(up_to as usize);
                    snapshot
                })
            };
            if let Some(snapshot) = snapshot {
                break snapshot;
            }
            changed.await;
        };
        self.copy.wait().await;
        activity.completed = true;
        if self.corrupt_copy.load(Ordering::Acquire) {
            return Ok(Box::pin(stream::iter([Ok(Bytes::from_static(
                b"invalid-service-snapshot",
            ))])));
        }
        let bytes = Bytes::from(serde_json::to_vec(&snapshot).map_err(failure)?);
        Ok(Box::pin(stream::iter([Ok(bytes)])))
    }

    async fn on_data_loss(&self) -> Result<bool> {
        self.recovery_calls.fetch_add(1, Ordering::SeqCst);
        self.recovery.wait().await;
        match self.recovery_outcome.load(Ordering::Acquire) {
            0 => Ok(false),
            1 => {
                let mut state = self.state.lock().unwrap();
                let mut next = state.clone();
                next.values.clear();
                self.persist(&next)?;
                *state = next;
                Ok(true)
            }
            2 => Err(failure("injected data-loss callback failure")),
            _ => Err(RuntimeError::ReplicaRebuildRequired(Box::new(
                ReplicaRebuildRequired::LocalProvider(LocalProviderRebuildRequired {
                    reason: ReplicaRebuildReason::IncompatibleRetainedHistory,
                    provider: ProviderReplicaIdentity {
                        replica: self.identity.lock().unwrap().clone().unwrap(),
                    },
                }),
            ))),
        }
    }
}

struct CountingFactory {
    calls: Arc<AtomicU64>,
    observed: Arc<Mutex<Vec<NativeCall>>>,
    provider: Arc<Provider>,
}

#[derive(Clone, Debug)]
enum NativeCall {
    Close,
    Abort,
    Role(Epoch, ReplicaRole),
    Epoch(Epoch),
    Current(ReplicaSetConfiguration),
    CatchUp(ReplicaSetConfiguration, ReplicaSetConfiguration),
    Wait(ReplicaSetQuorumMode, AccessStatus),
    FencedBegin(AccessStatus, String),
    Build(ReplicaInformation),
    RemoveBegin(ReplicaId, u64),
    RemoveEnd(ReplicaId, u64),
}

struct ObservedPrimary {
    #[allow(
        clippy::disallowed_types,
        reason = "delegates every protected callback to the same native owner"
    )]
    inner: Arc<dyn PrimaryReplicator>,
    state: Arc<dyn StateReplicator>,
    context: ReplicatorFactoryContext,
    observed: Arc<Mutex<Vec<NativeCall>>>,
    provider: Arc<Provider>,
}

#[async_trait]
impl Replicator for ObservedPrimary {
    async fn open(&self) -> Result<String> {
        self.inner.open().await
    }
    async fn close(&self) -> Result<()> {
        self.observed.lock().unwrap().push(NativeCall::Close);
        let synchronous = self.provider.synchronous_close.lock().unwrap().take();
        if let Some(blocked) = synchronous {
            self.provider.close_entered.notify_one();
            blocked
                .recv_timeout(Duration::from_secs(15))
                .map_err(failure)?;
            tokio::task::yield_now().await;
        }
        if self.provider.close_failure.swap(false, Ordering::AcqRel) {
            return Err(failure("injected replicator close failure"));
        }
        self.inner.close().await
    }
    fn abort(&self) {
        self.observed.lock().unwrap().push(NativeCall::Abort);
        self.inner.abort();
    }
    async fn current_progress(&self) -> Result<i64> {
        match self.provider.query_current.load(Ordering::Acquire) {
            1 => Err(RuntimeError::ProgressNotReady),
            2 => Err(failure("injected current-progress query failure")),
            _ => self.inner.current_progress().await,
        }
    }
    async fn catch_up_capability(&self) -> Result<i64> {
        match self.provider.query_capability.load(Ordering::Acquire) {
            1 => Err(RuntimeError::ProgressNotReady),
            2 => Err(failure("injected catch-up-capability query failure")),
            _ => self.inner.catch_up_capability().await,
        }
    }
    async fn change_role(&self, epoch: Epoch, role: ReplicaRole) -> Result<()> {
        self.inner.change_role(epoch, role).await?;
        self.observed
            .lock()
            .unwrap()
            .push(NativeCall::Role(epoch, role));
        Ok(())
    }
    async fn update_epoch(&self, epoch: Epoch) -> Result<()> {
        self.inner.update_epoch(epoch).await?;
        self.observed.lock().unwrap().push(NativeCall::Epoch(epoch));
        Ok(())
    }
}

#[async_trait]
impl PrimaryReplicator for ObservedPrimary {
    async fn on_data_loss(&self) -> Result<bool> {
        self.inner.on_data_loss().await
    }
    async fn update_current_replica_set_configuration(
        &self,
        current: ReplicaSetConfiguration,
    ) -> Result<()> {
        self.inner
            .update_current_replica_set_configuration(current.clone())
            .await?;
        self.observed
            .lock()
            .unwrap()
            .push(NativeCall::Current(current));
        Ok(())
    }
    async fn update_catch_up_replica_set_configuration(
        &self,
        current: ReplicaSetConfiguration,
        previous: ReplicaSetConfiguration,
    ) -> Result<()> {
        self.inner
            .update_catch_up_replica_set_configuration(current.clone(), previous.clone())
            .await?;
        self.observed
            .lock()
            .unwrap()
            .push(NativeCall::CatchUp(current, previous));
        Ok(())
    }
    async fn wait_for_catch_up_quorum(&self, mode: ReplicaSetQuorumMode) -> Result<()> {
        let status = self.context.write_status().await?;
        self.observed
            .lock()
            .unwrap()
            .push(NativeCall::Wait(mode, status));
        if self.provider.probe_fenced_admission.load(Ordering::Acquire)
            && status != AccessStatus::Granted
        {
            let progress = self.inner.current_progress().await?;
            let error = match self.state.begin_replicate(Bytes::new()) {
                Err(error) => error,
                Ok(admitted) => {
                    return Err(failure(format!(
                        "post-Revoke raw admission consumed LSN {}",
                        admitted.lsn
                    )));
                }
            };
            self.observed
                .lock()
                .unwrap()
                .push(NativeCall::FencedBegin(status, error.to_string()));
            if self.inner.current_progress().await? != progress {
                return Err(failure("rejected raw admission changed native progress"));
            }
        }
        let boundary = self.provider.catchup_boundary.swap(0, Ordering::AcqRel);
        let native = self.inner.wait_for_catch_up_quorum(mode);
        tokio::pin!(native);
        if boundary != 0 {
            assert!(
                futures::poll!(native.as_mut()).is_pending(),
                "native catch-up completed before required apply"
            );
            // Poll/enqueue the real wait before this same-facade round trip.
            // FIFO requests install Catchup before Progress; the selected
            // driver polls completion replies before later query requests.
            let progress = tokio::select! {
                biased;
                result = native.as_mut() => panic!("native wait completed before query barrier: {result:?}"),
                progress = self.inner.current_progress() => progress?,
            };
            assert!(progress < boundary as i64);
            assert!(
                futures::poll!(native.as_mut()).is_pending(),
                "native catch-up was ready at the owner query barrier"
            );
            self.provider.catchup_entered.notify_one();
            loop {
                tokio::select! {
                    biased;
                    result = native.as_mut() => return result,
                    () = self.provider.catchup_probe.notified() => {
                        // The native branch was just polled Pending, not the
                        // outer host operation or a new replacement wait.
                        self.provider.catchup_entered.notify_one();
                    }
                }
            }
        }
        native.await
    }
    async fn build_replica(&self, replica: ReplicaInformation) -> Result<()> {
        self.observed
            .lock()
            .unwrap()
            .push(NativeCall::Build(replica.clone()));
        self.inner.build_replica(replica).await
    }
    async fn remove_replica(&self, replica: ReplicaId) -> Result<()> {
        self.observed.lock().unwrap().push(NativeCall::RemoveBegin(
            replica,
            self.provider.copy_active.load(Ordering::Acquire),
        ));
        self.inner.remove_replica(replica).await?;
        self.observed.lock().unwrap().push(NativeCall::RemoveEnd(
            replica,
            self.provider.copy_active.load(Ordering::Acquire),
        ));
        Ok(())
    }
}

#[async_trait]
impl ReplicatorFactory for CountingFactory {
    async fn create_replicator(
        &self,
        context: ReplicatorFactoryContext,
        provider: Option<Arc<dyn StateProvider>>,
        settings: ReplicatorSettings,
    ) -> Result<ReplicatorInterfaces> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let native = DefaultReplicatorFactory::new()
            .create_replicator(context.clone(), provider, settings)
            .await?;
        let native = if self.provider.conservative_quorum.load(Ordering::Acquire) {
            kuberic_runtime::testing::without_catch_up_specific_quorum(native)?
        } else {
            native
        };
        let state = native.state_replicator();
        let primary = native.primary_replicator().unwrap();
        Ok(native.wrap_primary(
            Arc::new(ObservedPrimary {
                inner: primary,
                state: state.clone().unwrap(),
                context,
                observed: self.observed.clone(),
                provider: self.provider.clone(),
            }),
            state,
        ))
    }
}

struct PendingWrite {
    lsn: i64,
    outcome: oneshot::Receiver<Result<i64>>,
}

impl PendingWrite {
    async fn wait(self) -> Result<i64> {
        self.outcome.await.map_err(failure)?
    }
}

struct Application {
    owner: Mutex<std::sync::Weak<Self>>,
    provider: Arc<Provider>,
    address: String,
    partition: RwLock<Option<StatefulServicePartition>>,
    control: RwLock<Option<Arc<dyn Replicator>>>,
    state: RwLock<Option<Arc<dyn StateReplicator>>>,
    #[allow(clippy::disallowed_types)]
    primary: RwLock<Option<Arc<dyn kuberic_runtime::replicator::PrimaryReplicator>>>,
    role: Mutex<ReplicaRole>,
    tasks: Mutex<Vec<tokio::task::JoinHandle<()>>>,
    write_gate: Arc<AsyncMutex<()>>,
    stop: watch::Sender<bool>,
    failed: AtomicBool,
    close_failure: AtomicBool,
    closes: AtomicU64,
    aborts: AtomicU64,
    synchronous_close: Mutex<Option<std::sync::mpsc::Receiver<()>>>,
    close_entered: Notify,
    opens: AtomicU64,
    factories: Arc<AtomicU64>,
    observed: Arc<Mutex<Vec<NativeCall>>>,
    none_calls: AtomicU64,
    controlled: Mutex<Option<tokio::sync::mpsc::UnboundedSender<ReceivedOperation>>>,
}

impl Application {
    fn new(root: PathBuf, address: String) -> Result<Arc<Self>> {
        Ok(Arc::new(Self {
            owner: Mutex::new(std::sync::Weak::new()),
            provider: Arc::new(Provider::new(root)?),
            address,
            partition: RwLock::new(None),
            control: RwLock::new(None),
            state: RwLock::new(None),
            primary: RwLock::new(None),
            role: Mutex::new(ReplicaRole::None),
            tasks: Mutex::new(Vec::new()),
            write_gate: Arc::new(AsyncMutex::new(())),
            stop: watch::channel(false).0,
            failed: AtomicBool::new(false),
            close_failure: AtomicBool::new(false),
            closes: AtomicU64::new(0),
            aborts: AtomicU64::new(0),
            synchronous_close: Mutex::new(None),
            close_entered: Notify::new(),
            opens: AtomicU64::new(0),
            factories: Arc::new(AtomicU64::new(0)),
            observed: Arc::new(Mutex::new(Vec::new())),
            none_calls: AtomicU64::new(0),
            controlled: Mutex::new(None),
        }))
    }

    fn control_operations(&self) -> tokio::sync::mpsc::UnboundedReceiver<ReceivedOperation> {
        let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
        *self.controlled.lock().unwrap() = Some(sender);
        receiver
    }

    async fn begin_write(self: &Arc<Self>, data: Bytes) -> Result<PendingWrite> {
        let permit = self.write_gate.clone().lock_owned().await;
        if *self.stop.borrow() || self.failed.load(Ordering::Acquire) {
            return Err(RuntimeError::Closed);
        }
        let partition = self
            .partition
            .read()
            .await
            .clone()
            .ok_or(RuntimeError::NotOpen)?;
        let status = partition.get_write_status().await?;
        if status != AccessStatus::Granted {
            return Err(RuntimeError::WriteClosed(status));
        }
        let replicator = self
            .state
            .read()
            .await
            .clone()
            .ok_or(RuntimeError::NotOpen)?;
        let mut stop = self.stop.subscribe();
        let epoch = self.provider.state.lock().unwrap().epoch;
        let (reply, outcome) = oneshot::channel();
        let assigned = replicator.begin_replicate(data.clone())?;
        let lsn = assigned.lsn;
        let service = self.clone();
        self.tasks.lock().unwrap().push(tokio::spawn(async move {
            let _permit = permit;
            let work = async {
                assigned.completion.await?;
                service.provider.commit.wait().await;
                if service.provider.state.lock().unwrap().epoch != epoch {
                    return Err(failure("service epoch changed before local commitment"));
                }
                service.provider.commit(lsn, data)?;
                Ok(lsn)
            };
            let result = tokio::select! {
                biased;
                _ = stop.wait_for(|stopped| *stopped) => Err(RuntimeError::OperationCancelled),
                result = work => result,
            };
            if result.is_err() && !*stop.borrow() {
                service.failed.store(true, Ordering::Release);
                let _ = partition.report_fault(FaultType::Transient).await;
            }
            let _ = reply.send(result);
        }));
        Ok(PendingWrite { lsn, outcome })
    }

    async fn write(self: &Arc<Self>, data: impl Into<Bytes>) -> Result<i64> {
        self.begin_write(data.into()).await?.wait().await
    }

    async fn values(&self) -> Result<Vec<Bytes>> {
        if *self.stop.borrow() || self.failed.load(Ordering::Acquire) {
            return Err(RuntimeError::Closed);
        }
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
        Ok(self.provider.values())
    }

    fn spawn_consumer(
        self: &Arc<Self>,
        mut stream: OperationStream,
        mut copied: watch::Receiver<bool>,
        copy_done: Option<watch::Sender<bool>>,
        partition: StatefulServicePartition,
    ) {
        let service = self.clone();
        let mut stop = self.stop.subscribe();
        self.tasks.lock().unwrap().push(tokio::spawn(async move {
            let consume = async {
                let wait_for_copy = copy_done.is_none() && !*copied.borrow();
                if wait_for_copy && copied.wait_for(|copied| *copied).await.is_err() {
                    return Ok(());
                }
                let mut installed = false;
                let mut ended = false;
                loop {
                    let operation = match stream.get_operation().await {
                        Ok(Some(operation)) => operation,
                        Ok(None) => break,
                        Err(RuntimeError::Closed) => return Ok(()),
                        Err(error) => return Err(error),
                    };
                    ended |= operation.end_of_stream;
                    installed |= !operation.end_of_stream;
                    let controlled = service.controlled.lock().unwrap().clone();
                    if let Some(controlled) = controlled {
                        controlled.send(operation).map_err(failure)?;
                    } else {
                        service.provider.persist_received(&operation).await?;
                        operation.acknowledge()?;
                    }
                }
                if ended && let Some(copy_done) = copy_done {
                    if !installed {
                        return Err(failure("native copy ended without a service snapshot"));
                    }
                    copy_done.send_replace(true);
                }
                Ok::<_, RuntimeError>(())
            };
            tokio::select! {
                biased;
                _ = async { let _ = stop.wait_for(|stopped| *stopped).await; } => {}
                result = consume => {
                    if let Err(error) = result {
                        if matches!(error, RuntimeError::Closed) {
                            return;
                        }
                        service.failed.store(true, Ordering::Release);
                        let _ = stream.report_fault(error.to_string());
                        let _ = partition.report_fault(FaultType::Transient).await;
                    }
                }
            }
        }));
    }

    async fn drain(&self) {
        let tasks = std::mem::take(&mut *self.tasks.lock().unwrap());
        for task in &tasks {
            task.abort();
        }
        for task in tasks {
            if let Err(error) = task.await {
                assert!(error.is_cancelled(), "service task panicked: {error}");
            }
        }
    }
}

#[async_trait]
impl StatefulServiceReplica for Application {
    async fn open(self: Arc<Self>, context: OpenContext) -> Result<Arc<dyn Replicator>> {
        self.opens.fetch_add(1, Ordering::SeqCst);
        if let Some(control) = self.control.read().await.clone() {
            return Ok(control);
        }
        *self.provider.identity.lock().unwrap() = Some(context.identity);
        self.provider
            .persist(&self.provider.state.lock().unwrap())?;
        *self.partition.write().await = Some(context.partition.clone());
        let interfaces = context
            .partition
            .with_factory(Arc::new(CountingFactory {
                calls: self.factories.clone(),
                observed: self.observed.clone(),
                provider: self.provider.clone(),
            }))
            .create_replicator(
                Some(self.provider.clone()),
                Some(ReplicatorSettings {
                    replication_address: self.address.clone(),
                }),
            )
            .await?;
        let control = interfaces.replicator();
        *self.state.write().await = interfaces.state_replicator();
        *self.primary.write().await = interfaces.primary_replicator();
        *self.control.write().await = Some(control.clone());
        Ok(control)
    }

    async fn change_role(&self, role: ReplicaRole) -> Result<RoleChange> {
        if role == ReplicaRole::None {
            self.none_calls.fetch_add(1, Ordering::SeqCst);
        }
        let previous = *self.role.lock().unwrap();
        if role == ReplicaRole::Primary || previous == ReplicaRole::Primary {
            let _drain = self.write_gate.lock().await;
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
            // The task owner is retained by the service, including after a request
            // or stream handle is dropped. No runtime receipt owns service work.
            let owner = self.controlled_service_owner()?;
            if copy {
                owner.spawn_consumer(
                    state.copy_operations()?,
                    completion.clone(),
                    Some(copied),
                    partition.clone(),
                );
            }
            owner.spawn_consumer(state.replication_operations()?, completion, None, partition);
        }
        *self.role.lock().unwrap() = role;
        Ok(RoleChange {
            service_address: (role == ReplicaRole::Primary).then(|| "service/native-memory".into()),
        })
    }

    async fn close(&self) -> Result<()> {
        self.closes.fetch_add(1, Ordering::SeqCst);
        let synchronous = self.synchronous_close.lock().unwrap().take();
        if let Some(blocked) = synchronous {
            self.close_entered.notify_one();
            blocked
                .recv_timeout(Duration::from_secs(15))
                .map_err(failure)?;
            tokio::task::yield_now().await;
        }
        self.stop.send_replace(true);
        self.drain().await;
        self.state.write().await.take();
        self.primary.write().await.take();
        self.control.write().await.take();
        if self.close_failure.swap(false, Ordering::AcqRel) {
            return Err(failure("injected application close failure"));
        }
        Ok(())
    }

    fn abort(&self) {
        self.aborts.fetch_add(1, Ordering::SeqCst);
        self.stop.send_replace(true);
        for task in self.tasks.lock().unwrap().iter() {
            task.abort();
        }
    }
}

// change_role has the protected &self signature; a weak owner avoids an Arc cycle.
impl Application {
    fn controlled_service_owner(&self) -> Result<Arc<Self>> {
        self.owner
            .lock()
            .unwrap()
            .upgrade()
            .ok_or(RuntimeError::Closed)
    }
}

struct Resolver;

impl kuberic_runtime::host::ReplicaEndpointResolver for Resolver {
    fn control_endpoint(&self, _: &ReplicaIdentity) -> String {
        "http://127.0.0.1:1".into()
    }
    fn replication_endpoint(&self, _: &ReplicaIdentity) -> String {
        "http://127.0.0.1:1".into()
    }
}

struct Node {
    running: RunningReplica,
    control_address: String,
    service: Arc<Application>,
    identity: ReplicaIdentity,
    storage: ReplicaStorageIncarnationContext,
    history: HistoryContext,
    preview: PublicV1Identity,
    root: PathBuf,
}

impl Node {
    async fn start(root: &Path, resource: &ResourceUid, id: i64) -> Self {
        std::fs::create_dir_all(root).unwrap();
        let preview = PublicV1Identity::new(1);
        let pod = PodUid::new(format!("native-pod-{id}"));
        let pvc = PvcUid::new(format!("native-pvc-{id}"));
        let identity = ReplicaIdentity {
            replica_id: ReplicaId::new(id),
            instance_id: ReplicaInstanceId::new(pod.as_str()),
            agent_generation: AgentGeneration::new(format!("native-agent-{id}")),
        };
        let storage = ReplicaStorageIncarnationContext {
            replica: identity.clone(),
            storage: ReplicaStorageIdentity::Persistent {
                pod_uid: pod.clone(),
                pvc_uid: pvc.clone(),
            },
            storage_generation: StorageGeneration::new(format!("native-storage-{id}")),
        };
        let database = SqliteStore::metadata_database_path(root);
        if !database.exists() {
            drop(
                SqliteStore::create_public_v1(
                    &database,
                    preview.clone(),
                    StorageIdentity {
                        schema_version: SCHEMA_VERSION,
                        resource_uid: resource.clone(),
                        pod_uid: pod.clone(),
                        pvc_uid: pvc.clone(),
                        initialization_id: InitializationId::new(format!(
                            "native-initialization-{id}"
                        )),
                        local_identity: identity.clone(),
                        effective_policy: EffectivePolicy::fixed(1, 1).unwrap(),
                    },
                )
                .unwrap(),
            );
        }
        let fresh = !root.join("application/service.json").exists();
        let service =
            Application::new(root.join("application"), "http://127.0.0.1:0".into()).unwrap();
        *service.owner.lock().unwrap() = Arc::downgrade(&service);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let control_address = format!("http://{}", listener.local_addr().unwrap());
        let running = ReplicaHost::new(
            ReplicaProcessConfig {
                resource_uid: resource.clone(),
                replica_id: identity.replica_id,
                pod_uid: pod,
                pvc_uid: pvc,
                data_root: root.to_path_buf(),
                replication_address: "127.0.0.1:0".parse().unwrap(),
                bearer_token: TOKEN.into(),
                rpc_deadline: Duration::from_secs(5),
                transport_window_capacity: 16,
            },
            service.clone(),
            if fresh {
                ApplicationStorageState::FreshEmpty
            } else {
                ApplicationStorageState::Established
            },
            Arc::new(Resolver),
        )
        .with_public_v1_binding(PublicV1LifecycleBinding {
            identity: preview.clone(),
            resource_uid: resource.clone(),
            spec_generation: 1,
            state_persistence: StatePersistence::Persisted,
        })
        .start(listener)
        .await
        .unwrap();
        let initialization =
            derive_history_initialization_id(resource, PublicProtocolGeneration::new(1));
        let history =
            derive_bootstrap_history(resource, PublicProtocolGeneration::new(1), &initialization);
        running
            .handle()
            .initialize_public_v1_history(&initialization, &history, &storage)
            .await
            .unwrap();
        Self {
            running,
            control_address,
            service,
            identity,
            storage,
            history,
            preview,
            root: root.to_path_buf(),
        }
    }

    fn handle(&self) -> ReplicaHandle {
        self.running.handle()
    }

    async fn report(&self) -> PublicV1AgentReport {
        self.handle().public_v1_report().await.unwrap()
    }

    async fn intent(&self, program: PublicOperationProgram) -> PublicOperationIntent {
        let handle = self.handle();
        let next = handle.public_v1_next_operation_revision().await.unwrap();
        let revision = if matches!(
            program,
            PublicOperationProgram::Build(_) | PublicOperationProgram::Remove(_)
        ) {
            next - 1
        } else {
            next
        };
        let operation_id = match &program {
            PublicOperationProgram::Build(build) => build.attempt.clone(),
            _ => OperationId::new(format!(
                "native-operation-{}",
                NEXT_ID.fetch_add(1, Ordering::Relaxed)
            )),
        };
        let class = match &program {
            PublicOperationProgram::Swap { .. } => PublicOperationClass::PlannedSwap,
            PublicOperationProgram::Build(build) => PublicOperationClass::Build {
                target: build.replica.replica_id,
            },
            PublicOperationProgram::Remove(build) => PublicOperationClass::Remove {
                target: build.replica.replica_id,
            },
            PublicOperationProgram::Close => PublicOperationClass::Close,
            PublicOperationProgram::Abort => PublicOperationClass::Abort,
            _ => PublicOperationClass::Authority,
        };
        PublicOperationIntent {
            identity: self.preview.clone(),
            operation_id,
            revision,
            process_session_id: handle.process_session_id(),
            class,
            input_digest: program.digest(),
            replay_disposition: program.replay_disposition(),
            lifecycle: None,
            program: Some(program),
        }
    }

    async fn command(&self, program: PublicOperationProgram) -> kuberic_runtime::host::Result<()> {
        self.handle()
            .execute_public_v1(self.intent(program).await)
            .await
    }

    async fn open(&self, role: ReplicaRole, epoch: Epoch) {
        self.command(PublicOperationProgram::Open {
            replica: self.identity.clone(),
            existing: self.report().await.open_mode == OpenMode::Existing,
        })
        .await
        .unwrap();
        self.command(PublicOperationProgram::Role { epoch, role })
            .await
            .unwrap();
        if role == ReplicaRole::Primary {
            self.command(PublicOperationProgram::Epoch { epoch })
                .await
                .unwrap();
        }
    }

    async fn single(&self, epoch: Epoch) -> ConfigurationDescriptor {
        let current = ConfigurationDescriptor::new(
            epoch,
            self.identity.replica_id,
            vec![ConfigurationMember {
                identity: self.identity.clone(),
                role: ReplicaRole::Primary,
            }],
            1,
        );
        self.command(PublicOperationProgram::Configuration(PublicConfiguration {
            current: current.clone(),
            previous: None,
            replicas: Vec::new(),
        }))
        .await
        .unwrap();
        current
    }

    async fn descriptor(&self, role: ReplicaRole) -> PublicReplicaDescriptor {
        let report = self.report().await;
        PublicReplicaDescriptor {
            is_up: true,
            is_ready: true,
            build_id: OperationId::default(),
            identity: self.identity.clone(),
            replication_address: report.replication_address.unwrap(),
            process_session_id: report.process_session_id,
            storage_incarnation: Some(self.storage.clone()),
            role,
            current_progress: report.progress.current().expect("native progress is ready"),
            catch_up_capability: report
                .progress
                .capability()
                .expect("native capability is ready"),
            must_catchup: false,
        }
    }

    async fn build_intent(
        &self,
        target: &Self,
        current: &ConfigurationDescriptor,
    ) -> (PublicOperationIntent, PublicInboundBuildAuthority) {
        let report = target.report().await;
        let source_address = self.report().await.replication_address.unwrap();
        let attempt = OperationId::new(format!(
            "native-build-{}",
            NEXT_ID.fetch_add(1, Ordering::Relaxed)
        ));
        let intent = self
            .intent(PublicOperationProgram::Build(PublicBuildInput {
                attempt: attempt.clone(),
                epoch: current.epoch,
                current_configuration_id: Some(current.configuration_id.clone()),
                replica: target.identity.clone(),
                target_storage: target.storage.clone(),
                process_session_id: report.process_session_id.clone(),
                replication_address: report.replication_address.clone().unwrap(),
                history: Some(self.history.clone()),
                source_address: source_address.clone(),
            }))
            .await;
        let authority = PublicInboundBuildAuthority {
            operation_id: intent.operation_id.clone(),
            operation_revision: intent.revision,
            attempt: PublicBuildAttemptReference {
                resource_uid: self.history.resource_uid.clone(),
                epoch: current.epoch,
                build_id: attempt,
                source: self.identity.clone(),
                source_storage: self.storage.clone(),
                source_session: self.handle().process_session_id(),
                target: target.identity.clone(),
                target_storage: target.storage.clone(),
                target_session: report.process_session_id,
            },
            history: self.history.clone(),
            current_configuration_id: current.configuration_id.clone(),
            target_address: report.replication_address.unwrap(),
            source_address,
        };
        target
            .handle()
            .install_public_v1_inbound_build_authority(&authority)
            .await
            .unwrap();
        (intent, authority)
    }

    async fn build(
        &self,
        target: &Self,
        current: &ConfigurationDescriptor,
    ) -> PublicInboundBuildAuthority {
        let (intent, authority) = self.build_intent(target, current).await;
        self.handle().execute_public_v1(intent).await.unwrap();
        assert!(
            self.report()
                .await
                .completed_operations
                .iter()
                .any(|completion| {
                    completion.operation_id == authority.operation_id
                        && completion.result == PublicV1OperationResult::Succeeded
                })
        );
        authority
    }

    async fn stop(&mut self) {
        self.running.shutdown();
        match self.running.wait().await {
            Ok(()) => {}
            Err(kuberic_runtime::host::HostError::Runtime(RuntimeError::OperationCancelled)) => {
                let partition = self.service.partition.read().await.clone().unwrap();
                assert_ne!(
                    partition.get_read_status().await.unwrap(),
                    AccessStatus::Granted
                );
                assert_ne!(
                    partition.get_write_status().await.unwrap(),
                    AccessStatus::Granted
                );
                assert!(self.service.values().await.is_err());
            }
            Err(error) => panic!("unexpected host drainage failure: {error}"),
        }
        self.service.drain().await;
    }
}

struct Pair {
    source: Node,
    target: Node,
    configuration: ConfigurationDescriptor,
    authority: PublicInboundBuildAuthority,
}

impl Pair {
    async fn start(root: &Path, values: usize) -> Self {
        let resource = ResourceUid::new(format!(
            "native-resource-{}",
            NEXT_ID.fetch_add(1, Ordering::Relaxed)
        ));
        let source = Node::start(&root.join("source"), &resource, 1).await;
        let target = Node::start(&root.join("target"), &resource, 2).await;
        source.open(ReplicaRole::Primary, EPOCH).await;
        let single = source.single(EPOCH).await;
        for lsn in 1..=values {
            assert_eq!(
                source
                    .service
                    .write(Bytes::from(format!("value-{lsn}")))
                    .await
                    .unwrap(),
                lsn as i64
            );
        }
        target.open(ReplicaRole::IdleSecondary, EPOCH).await;
        let authority = source.build(&target, &single).await;
        let configuration = ConfigurationDescriptor::new(
            EPOCH,
            source.identity.replica_id,
            vec![
                ConfigurationMember {
                    identity: source.identity.clone(),
                    role: ReplicaRole::Primary,
                },
                ConfigurationMember {
                    identity: target.identity.clone(),
                    role: ReplicaRole::ActiveSecondary,
                },
            ],
            2,
        );
        target
            .command(PublicOperationProgram::Role {
                epoch: EPOCH,
                role: ReplicaRole::ActiveSecondary,
            })
            .await
            .unwrap();
        target
            .command(PublicOperationProgram::Epoch { epoch: EPOCH })
            .await
            .unwrap();
        for (local, remote, role) in [
            (&target, &source, ReplicaRole::Primary),
            (&source, &target, ReplicaRole::ActiveSecondary),
        ] {
            local
                .command(PublicOperationProgram::Configuration(PublicConfiguration {
                    current: configuration.clone(),
                    previous: None,
                    replicas: vec![remote.descriptor(role).await],
                }))
                .await
                .unwrap();
        }
        Self {
            source,
            target,
            configuration,
            authority,
        }
    }

    async fn stop(mut self) {
        self.source.stop().await;
        self.target.stop().await;
    }
}

async fn delivered(
    receiver: &mut tokio::sync::mpsc::UnboundedReceiver<ReceivedOperation>,
) -> ReceivedOperation {
    tokio::time::timeout(Duration::from_secs(10), receiver.recv())
        .await
        .unwrap()
        .unwrap()
}

async fn wire_report(node: &Node) -> PublicV1AgentReport {
    use kuberic_runtime::control::proto;
    let mut client =
        proto::agent_control_client::AgentControlClient::connect(node.control_address.clone())
            .await
            .unwrap();
    let mut request = Request::new(proto::GetAgentStatusRequest {
        protocol_version: kuberic_runtime::protocol::PROTOCOL_VERSION,
        resource_uid: node.history.resource_uid.to_string(),
        replica_id: node.identity.replica_id.value(),
        expected_instance_id: node.identity.instance_id.to_string(),
    });
    request
        .metadata_mut()
        .insert("authorization", format!("Bearer {TOKEN}").parse().unwrap());
    kuberic_runtime::control::normalize_public_v1_agent_status_report(
        client.get_status(request).await.unwrap().into_inner(),
    )
    .unwrap()
}

struct RenewalController {
    status: Mutex<Option<PublicV1SessionRenewalStatus>>,
    replacement: ReplicaHandle,
    endpoints: Vec<(ProcessSessionEndpoint, String)>,
}

#[async_trait]
impl PublicV1SessionRenewalApi for RenewalController {
    async fn observe_public_v1_session_renewal(
        &self,
        _: &PublicV1Scope,
    ) -> std::result::Result<Option<PublicV1SessionRenewalStatus>, String> {
        Ok(self.status.lock().unwrap().clone())
    }
    async fn persist_public_v1_session_renewal(
        &self,
        _: &PublicV1Scope,
        expected: Option<&PublicV1SessionRenewalStatus>,
        next: &PublicV1SessionRenewalStatus,
    ) -> std::result::Result<(), String> {
        if self.status.lock().unwrap().as_ref() != expected {
            return Err("stale controller renewal".into());
        }
        self.replacement
            .persist_public_v1_session_renewal(expected, next)
            .await
            .map_err(|error| error.to_string())?;
        *self.status.lock().unwrap() = Some(next.clone());
        Ok(())
    }
    async fn dispatch_public_v1_session_renewal(
        &self,
        scope: &PublicV1Scope,
        expected: &PublicV1SessionRenewalStatus,
        command: &ProtocolCommand,
    ) -> std::result::Result<(), String> {
        use kuberic_runtime::control::{convert, proto};
        if self.status.lock().unwrap().as_ref() != Some(expected) {
            return Err("stale controller dispatch".into());
        }
        let ProtocolCommand::PublicV1History(history) = command else {
            return Err("wrong command".into());
        };
        let PublicV1HistoryCommand::SessionRenewal { command: renewal } = history.as_ref() else {
            return Err("wrong history command".into());
        };
        let endpoint = renewal.expected_endpoint();
        let address = &self
            .endpoints
            .iter()
            .find(|(current, _)| {
                current.identity == endpoint.identity
                    && current.process_session_id == endpoint.process_session_id
                    && current.replicator_creation_id == endpoint.replicator_creation_id
                    && current.storage_incarnation == endpoint.storage_incarnation
            })
            .ok_or("captured survivor unreachable")?
            .1;
        let mut client = proto::replica_peer_client::ReplicaPeerClient::connect(address.clone())
            .await
            .map_err(|error| error.to_string())?;
        let mut request = Request::new(proto::ExecuteCommandRequest {
            protocol_version: kuberic_runtime::protocol::PROTOCOL_VERSION,
            resource_uid: scope.resource_uid.to_string(),
            target: Some(endpoint.identity.clone().into()),
            expected_process_session_id: endpoint.process_session_id.to_string(),
            command: Some(proto::execute_command_request::Command::PublicV1History(
                convert::public_v1_history_command_to_proto(history)
                    .map_err(|error| error.to_string())?,
            )),
        });
        request
            .metadata_mut()
            .insert("authorization", format!("Bearer {TOKEN}").parse().unwrap());
        client
            .execute(request)
            .await
            .map_err(|error| error.to_string())?;
        let next = match renewal.as_ref() {
            PublicV1SessionRenewalCommand::CloseAccessAndIngress { accepted } => {
                accepted.close_access()
            }
            PublicV1SessionRenewalCommand::UpdateEpochBarrier {
                accepted,
                survivor,
                epoch,
            } => accepted.record_epoch_updated(survivor, *epoch),
            PublicV1SessionRenewalCommand::ReplayReplacementRole {
                accepted,
                process_session_id,
                epoch,
                role,
            } => accepted.replay_replacement_role(process_session_id, *epoch, *role),
            PublicV1SessionRenewalCommand::InstallFreshConfiguration {
                accepted,
                configuration,
            } => accepted.install_fresh_configuration(configuration),
        }
        .map_err(str::to_owned)?;
        if matches!(
            renewal.as_ref(),
            PublicV1SessionRenewalCommand::UpdateEpochBarrier { .. }
        ) {
            self.replacement
                .persist_public_v1_session_renewal(Some(expected), &next)
                .await
                .map_err(|error| error.to_string())?;
        }
        *self.status.lock().unwrap() = Some(next);
        Ok(())
    }
}

async fn eventually(mut predicate: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(10), async {
        while !predicate() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct NativeEndpoint {
    identity: ReplicaIdentity,
    process_session_id: ProcessSessionId,
    storage: ReplicaStorageIncarnationContext,
    address: String,
}

impl NativeEndpoint {
    fn from_report(report: &PublicV1AgentReport) -> Self {
        Self {
            identity: report.replica.clone(),
            process_session_id: report.process_session_id.clone(),
            storage: report.storage_incarnation.clone(),
            address: report.replication_address.clone().unwrap(),
        }
    }
}

fn guid(domain: &[u8], bytes: &[u8]) -> Bytes {
    let mut digest = Sha256::new();
    digest.update((domain.len() as u64).to_be_bytes());
    digest.update(domain);
    digest.update(bytes);
    Bytes::copy_from_slice(&digest.finalize()[..16])
}

fn wire_endpoint(resource: &ResourceUid, endpoint: &NativeEndpoint) -> native_wire::Endpoint {
    native_wire::Endpoint {
        partition: guid(
            b"kuberic-classic-v1-partition",
            resource.as_str().as_bytes(),
        ),
        replica: endpoint.identity.replica_id.value(),
        incarnation: guid(
            b"kuberic-classic-v1-endpoint",
            &serde_json::to_vec(endpoint).unwrap(),
        ),
    }
}

async fn binding(pair: &Pair) -> serde_json::Value {
    serde_json::json!({
        "history": pair.source.history,
        "epoch": pair.configuration.epoch,
        "currentConfigurationId": pair.configuration.configuration_id,
        "previousConfigurationId": null,
        "primary": NativeEndpoint::from_report(&pair.source.report().await),
        "secondary": NativeEndpoint::from_report(&pair.target.report().await),
        "build": null,
    })
}

async fn replication_frame(pair: &Pair, lsn: i64, data: Bytes) -> native_wire::Envelope {
    let primary = NativeEndpoint::from_report(&pair.source.report().await);
    let secondary = NativeEndpoint::from_report(&pair.target.report().await);
    let epoch = native_wire::Epoch {
        data_loss: pair.configuration.epoch.data_loss_number,
        configuration: pair.configuration.epoch.configuration_number,
    };
    native_wire::Envelope {
        version: 1,
        source: Some(wire_endpoint(&pair.source.history.resource_uid, &primary)),
        destination: Some(wire_endpoint(&pair.source.history.resource_uid, &secondary)),
        sender_address: primary.address,
        message_id: uuid::Uuid::new_v4().to_string(),
        body: Some(native_wire::envelope::Body::ReplicationOperation(
            native_wire::ReplicationOperation {
                metadata: Some(native_wire::Metadata {
                    sequence: lsn,
                    operation_type: 1,
                    atomic_group: -1,
                }),
                original_epoch: Some(epoch),
                sending_epoch: Some(epoch),
                buffers: Some(native_wire::Buffers {
                    segments: vec![data],
                }),
                first_sequence: lsn,
                last_sequence: lsn,
                last_in_batch: lsn,
                buffer_counts: vec![1],
                completed_hint: 0,
            },
        )),
    }
}

async fn build_binding(
    source: &Node,
    target: &Node,
    authority: &PublicInboundBuildAuthority,
) -> serde_json::Value {
    serde_json::json!({
        "history": authority.history, "epoch": authority.attempt.epoch,
        "currentConfigurationId": authority.current_configuration_id,
        "previousConfigurationId": null,
        "primary": NativeEndpoint::from_report(&source.report().await),
        "secondary": NativeEndpoint::from_report(&target.report().await),
        "build": authority,
    })
}

async fn native_frame(
    source: &Node,
    target: &Node,
    body: native_wire::envelope::Body,
) -> native_wire::Envelope {
    let primary = NativeEndpoint::from_report(&source.report().await);
    let secondary = NativeEndpoint::from_report(&target.report().await);
    native_wire::Envelope {
        version: 1,
        source: Some(wire_endpoint(&source.history.resource_uid, &primary)),
        destination: Some(wire_endpoint(&source.history.resource_uid, &secondary)),
        sender_address: primary.address,
        message_id: uuid::Uuid::new_v4().to_string(),
        body: Some(body),
    }
}

async fn exchange(
    address: String,
    binding: &serde_json::Value,
    credential: &str,
    frames: Vec<native_wire::Envelope>,
) -> std::result::Result<tonic::Streaming<native_wire::Envelope>, tonic::Status> {
    let mut client = native_wire::carrier_client::CarrierClient::connect(address)
        .await
        .unwrap();
    let mut request = Request::new(stream::iter(frames).chain(stream::pending()));
    request
        .metadata_mut()
        .insert("authorization", credential.parse().unwrap());
    request.metadata_mut().insert_bin(
        "kuberic-native-binding-bin",
        tonic::metadata::MetadataValue::from_bytes(&serde_json::to_vec(binding).unwrap()),
    );
    client
        .exchange(request)
        .await
        .map(tonic::Response::into_inner)
}

async fn no_applied_ack(
    stream: &mut tonic::Streaming<native_wire::Envelope>,
    kind: OperationKind,
    through: i64,
) {
    let result = tokio::time::timeout(Duration::from_millis(100), async {
        loop {
            match stream.message().await {
                Ok(Some(envelope)) => {
                    if let Some(native_wire::envelope::Body::ReplicationAck(ack)) = envelope.body {
                        let applied = if kind == OperationKind::Replication {
                            ack.replication_applied
                        } else {
                            ack.copy_applied
                        };
                        assert!(
                            applied < through,
                            "unauthorized frame produced successful applied ACK: {ack:?}"
                        );
                    }
                }
                Ok(None) | Err(_) => return,
            }
        }
    })
    .await;
    let _ = result;
}

async fn applied(stream: &mut tonic::Streaming<native_wire::Envelope>, through: i64) {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let envelope = stream
                .message()
                .await
                .unwrap()
                .expect("native peer closed before ACK");
            if let Some(native_wire::envelope::Body::ReplicationAck(ack)) = envelope.body {
                assert_eq!(ack.error, 0, "native peer rejected the valid operation");
                if ack.replication_applied >= through {
                    break;
                }
            }
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn sf10_quiet_snapshot_build_activates_without_writes_or_fabricated_progress() {
    let root = root();
    for count in [0, 1] {
        let pair = Pair::start(&root.path().join(format!("boundary-{count}")), count).await;
        assert_eq!(
            pair.target.service.provider.values(),
            pair.source.service.provider.values()
        );
        assert_eq!(
            pair.target.service.provider.installs.load(Ordering::SeqCst),
            1
        );
        let report = pair.target.report().await;
        assert_eq!(report.progress.current(), Some(count as i64));
        assert_eq!(report.progress.capability(), Some(0));
        for mode in [
            kuberic_runtime::replicator::ReplicaSetQuorumMode::WriteQuorum,
            kuberic_runtime::replicator::ReplicaSetQuorumMode::All,
        ] {
            pair.source
                .service
                .primary
                .read()
                .await
                .clone()
                .unwrap()
                .wait_for_catch_up_quorum(mode)
                .await
                .unwrap();
        }
        pair.stop().await;
    }
}

#[tokio::test]
async fn hosted_operation_stream_replication_waits_for_service_persistence_and_ack() {
    let root = root();
    let pair = Pair::start(root.path(), 0).await;
    let mut operations = pair.target.service.control_operations();
    let mut write = pair
        .source
        .service
        .begin_write(Bytes::from_static(b"native-value"))
        .await
        .unwrap();
    let operation = delivered(&mut operations).await;
    assert_eq!(
        (operation.kind, operation.lsn, operation.end_of_stream),
        (OperationKind::Replication, write.lsn, false)
    );
    assert!(pair.target.service.provider.values().is_empty());
    pair.target
        .service
        .provider
        .persist_received(&operation)
        .await
        .unwrap();
    assert_eq!(
        pair.target.service.provider.values(),
        [Bytes::from_static(b"native-value")]
    );
    assert_eq!(pair.target.report().await.progress.current(), Some(1));
    assert!(
        matches!(
            write.outcome.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ),
        "native receive progress completed a service-ACK-dependent write"
    );
    operation.acknowledge().unwrap();
    assert_eq!(write.wait().await.unwrap(), 1);
    assert_eq!(
        pair.source.service.values().await.unwrap(),
        [Bytes::from_static(b"native-value")]
    );
    assert!(
        operations.try_recv().is_err(),
        "native quorum must not redeliver a synthetic commit watermark"
    );
    pair.stop().await;
}

#[tokio::test]
async fn hosted_operation_stream_copy_and_eof_wait_for_service_owned_snapshot_ack() {
    let root = root();
    let resource = ResourceUid::new("copy-service-ack");
    let mut source = Node::start(&root.path().join("source"), &resource, 1).await;
    let mut target = Node::start(&root.path().join("target"), &resource, 2).await;
    source.open(ReplicaRole::Primary, EPOCH).await;
    let single = source.single(EPOCH).await;
    source
        .service
        .write(Bytes::from_static(b"snapshot-value"))
        .await
        .unwrap();
    target.open(ReplicaRole::IdleSecondary, EPOCH).await;
    let mut operations = target.service.control_operations();
    let (intent, _) = source.build_intent(&target, &single).await;
    let handle = source.handle();
    let mut build = tokio::spawn(async move { handle.execute_public_v1(intent).await });
    let snapshot = delivered(&mut operations).await;
    assert_eq!(snapshot.kind, OperationKind::Copy);
    assert!(!snapshot.end_of_stream);
    assert!(target.service.provider.values().is_empty());
    target
        .service
        .provider
        .persist_received(&snapshot)
        .await
        .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(40), &mut build)
            .await
            .is_err()
    );
    snapshot.acknowledge().unwrap();
    let eof = delivered(&mut operations).await;
    assert_eq!(eof.kind, OperationKind::Copy);
    assert!(eof.end_of_stream);
    assert!(
        tokio::time::timeout(Duration::from_millis(40), &mut build)
            .await
            .is_err()
    );
    eof.acknowledge().unwrap();
    tokio::time::timeout(Duration::from_secs(10), build)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(
        target.service.provider.values(),
        [Bytes::from_static(b"snapshot-value")]
    );
    source.stop().await;
    target.stop().await;
}

#[tokio::test]
async fn sf10_snapshot_prefix_preserves_concurrent_and_later_exact_suffix() {
    let root = root();
    let resource = ResourceUid::new("copy-concurrent-suffix");
    let mut source = Node::start(&root.path().join("source"), &resource, 1).await;
    let mut target = Node::start(&root.path().join("target"), &resource, 2).await;
    source.open(ReplicaRole::Primary, EPOCH).await;
    let single = source.single(EPOCH).await;
    source
        .service
        .write(Bytes::from_static(b"snapshot"))
        .await
        .unwrap();
    target.open(ReplicaRole::IdleSecondary, EPOCH).await;
    source.service.provider.copy.hold();
    let (intent, _) = source.build_intent(&target, &single).await;
    let handle = source.handle();
    let build = tokio::spawn(async move { handle.execute_public_v1(intent).await });
    source.service.provider.copy.entered.notified().await;
    assert_eq!(
        source
            .service
            .write(Bytes::from_static(b"concurrent"))
            .await
            .unwrap(),
        2
    );
    source.service.provider.copy.release();
    tokio::time::timeout(Duration::from_secs(15), build)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    source
        .service
        .write(Bytes::from_static(b"later"))
        .await
        .unwrap();
    eventually(|| target.service.provider.values().len() == 3).await;
    assert_eq!(
        target.service.provider.values(),
        source.service.provider.values()
    );
    source.stop().await;
    target.stop().await;
}

#[tokio::test]
async fn hosted_operation_stream_drop_and_persistence_failure_are_not_successful_acks() {
    let root = root();
    for fail in [false, true] {
        let pair = Pair::start(&root.path().join(format!("fail-{fail}")), 0).await;
        let mut operations = pair.target.service.control_operations();
        let write = pair
            .source
            .service
            .begin_write(Bytes::from_static(b"held"))
            .await
            .unwrap();
        let operation = delivered(&mut operations).await;
        let mut observer = tokio::spawn(write.wait());
        if fail {
            pair.target
                .service
                .provider
                .fail_commit
                .store(true, Ordering::Release);
            assert!(
                pair.target
                    .service
                    .provider
                    .persist_received(&operation)
                    .await
                    .is_err()
            );
            pair.target
                .service
                .state
                .read()
                .await
                .clone()
                .unwrap()
                .replication_operations()
                .unwrap()
                .report_fault("service persistence failed".into())
                .unwrap();
        } else {
            drop(operation);
        }
        assert!(
            tokio::time::timeout(Duration::from_millis(60), &mut observer)
                .await
                .is_err()
        );
        assert!(pair.target.service.provider.values().is_empty());
        assert!(pair.source.service.provider.values().is_empty());
        pair.stop().await;
        assert!(
            tokio::time::timeout(Duration::from_secs(10), observer)
                .await
                .unwrap()
                .unwrap()
                .is_err()
        );
    }
}

#[tokio::test]
async fn primary_commit_failure_contains_the_ambiguous_native_quorum_success() {
    let root = root();
    let pair = Pair::start(root.path(), 0).await;
    pair.source
        .service
        .provider
        .fail_commit
        .store(true, Ordering::Release);
    assert!(
        pair.source
            .service
            .write(Bytes::from_static(b"ambiguous"))
            .await
            .is_err()
    );
    assert!(pair.source.service.provider.values().is_empty());
    assert_eq!(
        pair.target.service.provider.values(),
        [Bytes::from_static(b"ambiguous")]
    );
    assert!(pair.source.service.values().await.is_err());
    assert!(
        pair.source
            .service
            .write(Bytes::from_static(b"must-not-follow"))
            .await
            .is_err()
    );
    let report = pair.source.report().await;
    assert_ne!(report.read_status, AccessStatus::Granted);
    assert_ne!(report.write_status, AccessStatus::Granted);
    pair.stop().await;
}

#[tokio::test]
async fn native_transient_fault_restarts_same_storage_under_exact_session_renewal() {
    let root = root();
    let resource = ResourceUid::new("native-transient-fault-restart");
    let mut predecessor = Node::start(root.path(), &resource, 1).await;
    predecessor.open(ReplicaRole::Primary, EPOCH).await;
    predecessor.single(EPOCH).await;
    for value in [b"fault-prefix-1", b"fault-prefix-2", b"fault-prefix-3"] {
        predecessor
            .service
            .write(Bytes::copy_from_slice(value))
            .await
            .unwrap();
    }
    let acknowledged = predecessor.service.provider.values();
    let before = predecessor.report().await;
    assert_eq!(before.write_status, AccessStatus::Granted);
    let old_endpoint = predecessor
        .handle()
        .public_v1_process_endpoint(ReplicaRole::Primary)
        .await
        .unwrap();
    let old_handle = predecessor.handle();
    let old_partition = predecessor.service.partition.read().await.clone().unwrap();
    let old_intent = predecessor
        .intent(PublicOperationProgram::Progress { capability: false })
        .await;

    old_partition
        .report_fault(FaultType::Transient)
        .await
        .unwrap();
    assert_ne!(
        old_partition.get_read_status().await.unwrap(),
        AccessStatus::Granted
    );
    assert_ne!(
        old_partition.get_write_status().await.unwrap(),
        AccessStatus::Granted
    );
    assert!(predecessor.service.values().await.is_err());
    assert!(
        predecessor
            .service
            .write(Bytes::from_static(b"cannot-follow-transient-fault"))
            .await
            .is_err()
    );
    assert_eq!(predecessor.service.provider.values(), acknowledged);
    let connection =
        rusqlite::Connection::open(SqliteStore::metadata_database_path(root.path())).unwrap();
    let json: String = connection
        .query_row(
            "SELECT state_json FROM agent_state WHERE singleton=1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let durable: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(
        durable["reportedFault"],
        serde_json::to_value(FaultType::Transient).unwrap()
    );
    let faults = durable["publicV1OperationState"]["operations"]
        .as_object()
        .unwrap()
        .values()
        .filter(|record| {
            record["intent"]["class"]
                == serde_json::to_value(PublicOperationClass::TransientFault).unwrap()
        })
        .collect::<Vec<_>>();
    assert_eq!(
        faults.len(),
        1,
        "actual fault did not request exact durable containment"
    );
    assert_eq!(
        faults[0]["intent"]["processSessionId"],
        serde_json::to_value(&before.process_session_id).unwrap()
    );
    assert_eq!(
        faults[0]["acceptedStorage"],
        serde_json::to_value(&before.storage_incarnation).unwrap()
    );
    assert_eq!(
        faults[0]["acceptedHistory"],
        serde_json::to_value(&before.history).unwrap()
    );
    drop(connection);

    // Reap the process only after its actual fault request and serving fence.
    predecessor.stop().await;
    assert_eq!(
        old_handle
            .public_v1_raw_retained_task_cardinality()
            .unwrap(),
        (0, 0)
    );
    assert_ne!(
        old_partition.get_write_status().await.unwrap(),
        AccessStatus::Granted
    );
    let mut replacement = Node::start(root.path(), &resource, 1).await;
    let fresh = replacement.report().await;
    assert_eq!(fresh.replica, before.replica);
    assert_eq!(fresh.storage_incarnation, before.storage_incarnation);
    assert_eq!(fresh.history, before.history);
    assert_eq!(fresh.open_mode, OpenMode::Existing);
    assert_ne!(fresh.process_session_id, before.process_session_id);
    assert_ne!(fresh.replicator_creation_id, before.replicator_creation_id);
    assert_eq!(replacement.service.opens.load(Ordering::SeqCst), 0);
    assert_eq!(replacement.service.provider.values(), acknowledged);
    assert_ne!(fresh.read_status, AccessStatus::Granted);
    assert_ne!(fresh.write_status, AccessStatus::Granted);
    let endpoint = replacement
        .handle()
        .public_v1_process_endpoint(ReplicaRole::None)
        .await
        .unwrap();
    let epoch = Epoch::new(0, 2);
    let configuration = ConfigurationDescriptor::new(
        epoch,
        replacement.identity.replica_id,
        vec![ConfigurationMember {
            identity: replacement.identity.clone(),
            role: ReplicaRole::Primary,
        }],
        1,
    );
    let required = PublicV1SessionRenewalStatus::required(ProcessSessionRenewalRequired {
        operation_id: OperationId::new("fault-requested-exact-session-renewal"),
        reason: ProcessSessionRenewalReason::PrimaryReplacement,
        resource_uid: resource.clone(),
        history: before.history.clone(),
        previous_epoch: EPOCH,
        renewal_epoch: epoch,
        predecessor: old_endpoint,
        replacement: Some(endpoint.clone()),
        surviving_sessions: Vec::new(),
        fresh_configuration: configuration,
        predecessor_contained: true,
        data_loss: None,
    })
    .unwrap();
    required
        .required
        .validate_replacement_report(&fresh)
        .unwrap();
    let controller = RenewalController {
        status: Mutex::new(None),
        replacement: replacement.handle(),
        endpoints: vec![(endpoint, replacement.control_address.clone())],
    };
    let scope = PublicV1Scope {
        namespace: "testing".into(),
        name: "transient-fault-restart".into(),
        resource_uid: resource,
    };
    controller
        .persist_public_v1_session_renewal(&scope, None, &required)
        .await
        .unwrap();
    let mut authorized = false;
    for _ in 0..24 {
        let current = controller
            .observe_public_v1_session_renewal(&scope)
            .await
            .unwrap()
            .unwrap();
        let plan = plan_public_v1_session_renewal(&current, &[]).unwrap();
        if matches!(plan, PublicV1SessionRenewalPlan::Stable) {
            assert_eq!(current.stage, PublicSessionRenewalStage::Authorized);
            authorized = true;
            break;
        }
        let mut replaying_role = false;
        if let PublicV1SessionRenewalPlan::Dispatch {
            command: ProtocolCommand::PublicV1History(history),
            ..
        } = &plan
            && let PublicV1HistoryCommand::SessionRenewal { command } = history.as_ref()
        {
            match command.as_ref() {
                PublicV1SessionRenewalCommand::ReplayReplacementRole { .. } => {
                    replaying_role = true;
                    assert_eq!(replacement.service.opens.load(Ordering::SeqCst), 0);
                    assert_ne!(
                        replacement.report().await.write_status,
                        AccessStatus::Granted
                    );
                }
                PublicV1SessionRenewalCommand::InstallFreshConfiguration {
                    configuration, ..
                } => {
                    replacement
                        .command(PublicOperationProgram::Configuration(PublicConfiguration {
                            current: configuration.clone(),
                            previous: None,
                            replicas: Vec::new(),
                        }))
                        .await
                        .unwrap();
                }
                _ => {}
            }
        }
        execute_public_v1_session_renewal(&controller, &scope, plan)
            .await
            .unwrap();
        if replaying_role {
            let opened = replacement.report().await;
            assert_eq!(replacement.service.opens.load(Ordering::SeqCst), 1);
            assert_eq!(opened.process_session_id, fresh.process_session_id);
            assert_eq!(opened.storage_incarnation, fresh.storage_incarnation);
            assert_ne!(opened.write_status, AccessStatus::Granted);
            assert_eq!(
                opened.replicator_creation_id, fresh.replicator_creation_id,
                "automatic Open changed the already-captured renewal endpoint creation"
            );
        }
    }
    assert!(
        authorized,
        "actual transient fault never reached authorized restart"
    );
    let recovered = wire_report(&replacement).await;
    assert_eq!(recovered.process_session_id, fresh.process_session_id);
    assert_eq!(
        recovered.replicator_creation_id,
        fresh.replicator_creation_id
    );
    assert_eq!(recovered.storage_incarnation, before.storage_incarnation);
    assert_eq!(recovered.history, before.history);
    assert_eq!(recovered.role, ReplicaRole::Primary);
    assert_eq!(replacement.service.opens.load(Ordering::SeqCst), 1);
    assert_eq!(replacement.service.values().await.unwrap(), acknowledged);
    let calls = replacement.service.observed.lock().unwrap().len();
    assert!(
        replacement
            .handle()
            .execute_public_v1(old_intent)
            .await
            .is_err()
    );
    assert!(
        old_partition
            .report_fault(FaultType::Transient)
            .await
            .is_err()
    );
    assert_eq!(replacement.service.observed.lock().unwrap().len(), calls);
    assert_eq!(
        replacement.report().await.write_status,
        AccessStatus::Granted
    );
    assert_eq!(
        replacement
            .service
            .write(Bytes::from_static(b"after-transient-restart"))
            .await
            .unwrap(),
        4
    );
    assert_eq!(
        &replacement.service.provider.values()[..3],
        acknowledged.as_slice()
    );
    replacement.stop().await;
}

#[tokio::test]
async fn native_shared_stream_handles_do_not_rewind_or_reapply_acknowledged_operations() {
    let root = root();
    let pair = Pair::start(root.path(), 0).await;
    let mut operations = pair.target.service.control_operations();
    let write = pair
        .source
        .service
        .begin_write(Bytes::from_static(b"once"))
        .await
        .unwrap();
    let operation = delivered(&mut operations).await;
    let state = pair.target.service.state.read().await.clone().unwrap();
    let mut another = state.replication_operations().unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(40), another.get_operation())
            .await
            .is_err()
    );
    pair.target
        .service
        .provider
        .persist_received(&operation)
        .await
        .unwrap();
    operation.acknowledge().unwrap();
    write.wait().await.unwrap();
    assert_eq!(pair.target.service.provider.values().len(), 1);
    assert!(operations.try_recv().is_err());
    pair.stop().await;
}

#[tokio::test]
async fn production_carrier_rejects_crossed_process_storage_history_epoch_and_endpoint() {
    let root = root();
    let pair = Pair::start(root.path(), 1).await;
    let valid = binding(&pair).await;
    let address = pair.target.report().await.replication_address.unwrap();
    let mut rejected = Vec::new();
    for (side, field) in [
        ("primary", "processSessionId"),
        ("secondary", "processSessionId"),
        ("primary", "address"),
        ("secondary", "address"),
    ] {
        let mut crossed = valid.clone();
        crossed[side][field] = "stale".into();
        rejected.push(crossed);
    }
    let mut crossed = valid.clone();
    crossed["primary"]["storage"]["storageGeneration"] = "retired".into();
    rejected.push(crossed);
    let mut crossed = valid.clone();
    crossed["history"]["historyId"] = "predecessor".into();
    rejected.push(crossed);
    let mut crossed = valid.clone();
    crossed["epoch"]["configurationNumber"] = 99.into();
    rejected.push(crossed);
    let mut crossed = valid.clone();
    crossed["currentConfigurationId"] = "stale-configuration".into();
    rejected.push(crossed);
    for crossed in rejected {
        let error = exchange(
            address.clone(),
            &crossed,
            &format!("Bearer {TOKEN}"),
            Vec::new(),
        )
        .await
        .unwrap_err();
        assert_eq!(error.code(), tonic::Code::PermissionDenied);
    }
    let error = exchange(address, &valid, "Bearer wrong", Vec::new())
        .await
        .unwrap_err();
    assert_eq!(error.code(), tonic::Code::Unauthenticated);
    assert_eq!(
        pair.target.service.provider.values(),
        [Bytes::from_static(b"value-1")]
    );
    pair.source
        .service
        .write(Bytes::from_static(b"unaffected"))
        .await
        .unwrap();
    assert_eq!(pair.target.service.provider.values().len(), 2);
    pair.stop().await;
}

#[tokio::test]
async fn three_replica_native_quorum_ignores_hung_minority_and_rejects_secondary_origin() {
    let root = root();
    let mut pair = Pair::start(root.path(), 1).await;
    let mut third = Node::start(
        &root.path().join("third"),
        &pair.source.history.resource_uid,
        3,
    )
    .await;
    third.open(ReplicaRole::IdleSecondary, EPOCH).await;
    pair.source.build(&third, &pair.configuration).await;
    third
        .command(PublicOperationProgram::Role {
            epoch: EPOCH,
            role: ReplicaRole::ActiveSecondary,
        })
        .await
        .unwrap();
    third
        .command(PublicOperationProgram::Epoch { epoch: EPOCH })
        .await
        .unwrap();
    let current = ConfigurationDescriptor::new(
        EPOCH,
        pair.source.identity.replica_id,
        vec![
            ConfigurationMember {
                identity: pair.source.identity.clone(),
                role: ReplicaRole::Primary,
            },
            ConfigurationMember {
                identity: pair.target.identity.clone(),
                role: ReplicaRole::ActiveSecondary,
            },
            ConfigurationMember {
                identity: third.identity.clone(),
                role: ReplicaRole::ActiveSecondary,
            },
        ],
        2,
    );
    let entered = Arc::new(Notify::new());
    let observed = entered.clone();
    let isolated = third.identity.instance_id.to_string();
    let route = TestingTcpRoute::register_with_kind(
        pair.source.identity.instance_id.to_string(),
        move |kind, destination, endpoint| {
            let hung = kind == TestingTcpRouteKind::Replication && destination == isolated;
            let entered = observed.clone();
            Box::pin(async move {
                if hung {
                    entered.notify_one();
                    std::future::pending::<std::io::Result<String>>().await
                } else {
                    Ok(endpoint)
                }
            })
        },
    )
    .unwrap();
    for local in [&pair.target, &third, &pair.source] {
        let mut replicas = Vec::new();
        for peer in [&pair.source, &pair.target, &third] {
            if peer.identity != local.identity {
                replicas.push(
                    peer.descriptor(if peer.identity == pair.source.identity {
                        ReplicaRole::Primary
                    } else {
                        ReplicaRole::ActiveSecondary
                    })
                    .await,
                );
            }
        }
        local
            .command(PublicOperationProgram::Configuration(PublicConfiguration {
                current: current.clone(),
                previous: None,
                replicas,
            }))
            .await
            .unwrap();
    }
    pair.configuration = current;
    let mut rogue = binding(&pair).await;
    rogue["primary"] =
        serde_json::to_value(NativeEndpoint::from_report(&third.report().await)).unwrap();
    let error = exchange(
        pair.target.report().await.replication_address.unwrap(),
        &rogue,
        &format!("Bearer {TOKEN}"),
        Vec::new(),
    )
    .await
    .unwrap_err();
    assert_eq!(error.code(), tonic::Code::PermissionDenied);
    let write = pair
        .source
        .service
        .begin_write(Bytes::from_static(b"live-majority"))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(10), entered.notified())
        .await
        .unwrap();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(20), write.wait())
            .await
            .unwrap()
            .unwrap(),
        2
    );
    assert_eq!(pair.target.service.provider.values().len(), 2);
    assert_eq!(third.service.provider.values().len(), 1);
    tokio::time::timeout(
        Duration::from_secs(10),
        pair.source.command(PublicOperationProgram::Close),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(
        pair.source
            .handle()
            .public_v1_raw_retained_task_cardinality()
            .unwrap(),
        (0, 0)
    );
    drop(route);
    pair.source.stop().await;
    pair.target.stop().await;
    third.stop().await;
}

#[tokio::test]
async fn native_reconnect_does_not_transfer_dequeued_service_work_to_an_rpc_waiter() {
    let root = root();
    let pair = Pair::start(root.path(), 0).await;
    let mut operations = pair.target.service.control_operations();
    let frame = replication_frame(&pair, 1, Bytes::from_static(b"reconnect")).await;
    let binding = binding(&pair).await;
    let address = pair.target.report().await.replication_address.unwrap();
    let first = exchange(
        address.clone(),
        &binding,
        &format!("Bearer {TOKEN}"),
        vec![frame.clone()],
    )
    .await
    .unwrap();
    let operation = delivered(&mut operations).await;
    drop(first);
    let mut second = exchange(address, &binding, &format!("Bearer {TOKEN}"), vec![frame])
        .await
        .unwrap();
    pair.target
        .service
        .provider
        .persist_received(&operation)
        .await
        .unwrap();
    operation.acknowledge().unwrap();
    applied(&mut second, 1).await;
    assert!(
        operations.try_recv().is_err(),
        "reconnect redelivered already dequeued service work"
    );
    assert_eq!(
        pair.target.service.provider.values(),
        [Bytes::from_static(b"reconnect")]
    );
    drop(second);
    pair.stop().await;
}

#[tokio::test]
async fn native_carrier_bad_payload_and_wrong_sender_cannot_publish_service_progress() {
    let root = root();
    for wrong_sender in [false, true] {
        let pair = Pair::start(&root.path().join(format!("sender-{wrong_sender}")), 0).await;
        let mut frame = replication_frame(&pair, 1, Bytes::from_static(b"bad")).await;
        if wrong_sender {
            frame.source.as_mut().unwrap().replica = 3;
        } else if let Some(native_wire::envelope::Body::ReplicationOperation(body)) =
            &mut frame.body
        {
            body.buffer_counts = vec![2];
        }
        let result = exchange(
            pair.target.report().await.replication_address.unwrap(),
            &binding(&pair).await,
            &format!("Bearer {TOKEN}"),
            vec![frame],
        )
        .await;
        if let Ok(mut stream) = result {
            tokio::time::timeout(Duration::from_secs(10), async {
                while stream.message().await.unwrap_or(None).is_some() {}
            })
            .await
            .unwrap();
        }
        assert!(pair.target.service.provider.values().is_empty());
        assert_eq!(pair.target.report().await.progress.current(), Some(0));
        pair.stop().await;
    }
}

#[tokio::test]
async fn successor_inbound_build_retires_native_receiver_instead_of_resetting_metadata() {
    let root = root();
    let pair = Pair::start(root.path(), 1).await;
    pair.target
        .handle()
        .install_public_v1_inbound_build_authority(&pair.authority)
        .await
        .unwrap();
    let mut stale = pair.authority.clone();
    stale.operation_revision = 0;
    assert!(
        pair.target
            .handle()
            .install_public_v1_inbound_build_authority(&stale)
            .await
            .is_err()
    );
    assert_eq!(
        pair.target.service.provider.values(),
        [Bytes::from_static(b"value-1")]
    );
    let mut successor = pair.authority.clone();
    successor.operation_revision += 100;
    successor.operation_id = OperationId::new("successor-copy");
    successor.attempt.build_id = successor.operation_id.clone();
    let error = pair
        .target
        .handle()
        .install_public_v1_inbound_build_authority(&successor)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("requires process replacement"));
    let report = pair.target.report().await;
    assert_ne!(report.read_status, AccessStatus::Granted);
    assert_ne!(report.write_status, AccessStatus::Granted);
    assert_eq!(
        pair.target.service.provider.values(),
        [Bytes::from_static(b"value-1")]
    );
    pair.stop().await;
}

#[tokio::test]
async fn successor_copy_requires_replacement_without_exposing_held_service_install() {
    let root = root();
    let resource = ResourceUid::new("successor-held-install");
    let mut source = Node::start(&root.path().join("source"), &resource, 1).await;
    let mut target = Node::start(&root.path().join("target"), &resource, 2).await;
    source.open(ReplicaRole::Primary, EPOCH).await;
    let single = source.single(EPOCH).await;
    source
        .service
        .write(Bytes::from_static(b"held-snapshot"))
        .await
        .unwrap();
    target.open(ReplicaRole::IdleSecondary, EPOCH).await;
    target.service.provider.install.hold();
    let (intent, mut authority) = source.build_intent(&target, &single).await;
    let handle = source.handle();
    let build = tokio::spawn(async move { handle.execute_public_v1(intent).await });
    tokio::time::timeout(
        Duration::from_secs(10),
        target.service.provider.install.entered.notified(),
    )
    .await
    .unwrap();
    authority.operation_id = OperationId::new("held-install-successor");
    authority.operation_revision += 100;
    authority.attempt.build_id = authority.operation_id.clone();
    assert!(
        target
            .handle()
            .install_public_v1_inbound_build_authority(&authority)
            .await
            .unwrap_err()
            .to_string()
            .contains("requires process replacement")
    );
    assert_ne!(target.report().await.read_status, AccessStatus::Granted);
    assert_ne!(target.report().await.write_status, AccessStatus::Granted);
    assert!(target.service.values().await.is_err());
    assert!(
        target
            .command(PublicOperationProgram::Open {
                replica: target.identity.clone(),
                existing: true,
            })
            .await
            .is_err()
    );
    target.service.provider.install.release();
    tokio::time::sleep(Duration::from_millis(60)).await;
    assert_ne!(target.report().await.read_status, AccessStatus::Granted);
    tokio::time::timeout(Duration::from_secs(10), source.stop())
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(10), target.stop())
        .await
        .unwrap();
    let retained = target.service.provider.values();
    tokio::time::sleep(Duration::from_millis(40)).await;
    assert_eq!(target.service.provider.values(), retained);
    assert!(
        tokio::time::timeout(Duration::from_secs(10), build)
            .await
            .unwrap()
            .unwrap()
            .is_err()
    );
}

#[tokio::test]
async fn service_rejects_corrupt_copy_and_conflicting_retained_history_before_ack() {
    let root = root();
    for corrupt in [false, true] {
        let directory = root.path().join(format!("corrupt-{corrupt}"));
        let resource = ResourceUid::new(format!("retained-conflict-{corrupt}"));
        let mut source = Node::start(&directory.join("source"), &resource, 1).await;
        let mut target = Node::start(&directory.join("target"), &resource, 2).await;
        let epoch = Epoch::new(0, 2);
        source.open(ReplicaRole::Primary, epoch).await;
        let single = source.single(epoch).await;
        source
            .service
            .write(Bytes::from_static(b"source-history"))
            .await
            .unwrap();
        if !corrupt {
            target.open(ReplicaRole::Primary, EPOCH).await;
            target.single(EPOCH).await;
            target
                .service
                .write(Bytes::from_static(b"acknowledged-conflicting-history"))
                .await
                .unwrap();
            target.stop().await;
            target = Node::start(&directory.join("target"), &resource, 2).await;
        }
        target.open(ReplicaRole::IdleSecondary, epoch).await;
        source
            .service
            .provider
            .corrupt_copy
            .store(corrupt, Ordering::Release);
        let before = target.service.provider.values();
        let bytes = std::fs::read(target.service.provider.root.join("service.json")).unwrap();
        let (intent, _) = source.build_intent(&target, &single).await;
        let operation_id = intent.operation_id.clone();
        let handle = source.handle();
        let build = tokio::spawn(async move { handle.execute_public_v1(intent).await });
        eventually(|| target.service.failed.load(Ordering::Acquire)).await;
        assert_eq!(target.service.provider.values(), before);
        assert_eq!(
            std::fs::read(target.service.provider.root.join("service.json")).unwrap(),
            bytes
        );
        assert!(
            !source
                .report()
                .await
                .completed_operations
                .iter()
                .any(|completion| {
                    completion.operation_id == operation_id
                        && completion.result == PublicV1OperationResult::Succeeded
                })
        );
        assert_ne!(target.report().await.write_status, AccessStatus::Granted);
        tokio::time::timeout(Duration::from_secs(10), source.stop())
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(10), target.stop())
            .await
            .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_secs(10), build)
                .await
                .unwrap()
                .unwrap()
                .is_err()
        );
    }
}

#[tokio::test]
async fn hosted_operation_stream_close_and_abort_drain_with_dequeued_or_unconsumed_work() {
    let root = root();
    for abort in [false, true] {
        let pair = Pair::start(&root.path().join(format!("abort-{abort}")), 0).await;
        let mut operations = pair.target.service.control_operations();
        let write = pair
            .source
            .service
            .begin_write(Bytes::from_static(b"pending"))
            .await
            .unwrap();
        let operation = delivered(&mut operations).await;
        let program = if abort {
            PublicOperationProgram::Abort
        } else {
            PublicOperationProgram::Close
        };
        let intent = pair.target.intent(program).await;
        let handle = pair.target.handle();
        let mut terminal = tokio::spawn(async move { handle.execute_public_v1(intent).await });
        if abort {
            tokio::time::timeout(Duration::from_secs(10), terminal)
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            let _ = operation.acknowledge();
            assert!(pair.target.service.provider.values().is_empty());
            tokio::time::sleep(Duration::from_millis(40)).await;
            assert!(pair.source.service.provider.values().is_empty());
            assert_ne!(
                pair.target.report().await.write_status,
                AccessStatus::Granted
            );
        } else {
            assert!(
                tokio::time::timeout(Duration::from_millis(40), &mut terminal)
                    .await
                    .is_err()
            );
            pair.target
                .service
                .provider
                .persist_received(&operation)
                .await
                .unwrap();
            operation.acknowledge().unwrap();
            tokio::time::timeout(Duration::from_secs(10), async {
                loop {
                    tokio::select! {
                        result = &mut terminal => { result.unwrap().unwrap(); break; }
                        operation = operations.recv() => {
                            let operation = operation.unwrap();
                            assert!(operation.end_of_stream);
                            operation.acknowledge().unwrap();
                        }
                    }
                }
            })
            .await
            .unwrap();
        }
        assert_eq!(pair.target.service.none_calls.load(Ordering::SeqCst), 0);
        assert_eq!(
            pair.target
                .handle()
                .public_v1_raw_retained_task_cardinality()
                .unwrap(),
            (0, 0)
        );
        drop(write);
        pair.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hosted_sf06_close_waits_for_synchronous_service_effect_even_if_observer_is_dropped() {
    let root = root();
    let resource = ResourceUid::new("synchronous-copy");
    let mut source = Node::start(&root.path().join("source"), &resource, 1).await;
    let mut target = Node::start(&root.path().join("target"), &resource, 2).await;
    source.open(ReplicaRole::Primary, EPOCH).await;
    let single = source.single(EPOCH).await;
    source
        .service
        .write(Bytes::from_static(b"copied"))
        .await
        .unwrap();
    target.open(ReplicaRole::IdleSecondary, EPOCH).await;
    let (release, blocked) = std::sync::mpsc::channel();
    *target.service.provider.synchronous_install.lock().unwrap() = Some(blocked);
    let (intent, _) = source.build_intent(&target, &single).await;
    let handle = source.handle();
    let build = tokio::spawn(async move { handle.execute_public_v1(intent).await });
    target.service.provider.synchronous_entered.notified().await;
    let close = target.intent(PublicOperationProgram::Close).await;
    let handle = target.handle();
    let mut observer = tokio::spawn(async move { handle.execute_public_v1(close).await });
    assert!(
        tokio::time::timeout(Duration::from_millis(60), &mut observer)
            .await
            .is_err()
    );
    observer.abort();
    let _ = observer.await;
    release.send(()).unwrap();
    eventually(|| target.service.provider.installs.load(Ordering::SeqCst) == 1).await;
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if target
                .handle()
                .public_v1_raw_retained_task_cardinality()
                .unwrap()
                == (0, 0)
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let after = target.service.provider.values();
    tokio::time::sleep(Duration::from_millis(40)).await;
    assert_eq!(target.service.provider.values(), after);
    build.abort();
    let _ = build.await;
    source.stop().await;
    target.stop().await;
}

type HungCarrierStream =
    Pin<Box<dyn Stream<Item = std::result::Result<native_wire::Envelope, tonic::Status>> + Send>>;

struct HungCarrier {
    entered: Arc<Notify>,
}

#[tonic::async_trait]
impl native_wire::carrier_server::Carrier for HungCarrier {
    type ExchangeStream = HungCarrierStream;
    async fn exchange(
        &self,
        request: Request<tonic::Streaming<native_wire::Envelope>>,
    ) -> std::result::Result<tonic::Response<Self::ExchangeStream>, tonic::Status> {
        assert!(
            request
                .metadata()
                .get_bin("kuberic-native-binding-bin")
                .is_some()
        );
        let mut incoming = request.into_inner();
        let first = incoming.message().await?.unwrap();
        assert!(matches!(
            first.body,
            Some(native_wire::envelope::Body::StartCopy(_))
        ));
        self.entered.notify_one();
        Ok(tonic::Response::new(Box::pin(stream::unfold(
            incoming,
            |incoming| async move {
                std::future::pending::<()>().await;
                Some((Ok(native_wire::Envelope::default()), incoming))
            },
        ))))
    }
}

#[tokio::test]
async fn hung_native_build_is_drained_by_successor_close_without_a_fake_rpc_deadline() {
    let root = root();
    let resource = ResourceUid::new("hung-native-carrier");
    let mut source = Node::start(&root.path().join("source"), &resource, 1).await;
    let mut target = Node::start(&root.path().join("target"), &resource, 2).await;
    source.open(ReplicaRole::Primary, EPOCH).await;
    let single = source.single(EPOCH).await;
    target.open(ReplicaRole::IdleSecondary, EPOCH).await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let destination = format!("http://{}", listener.local_addr().unwrap());
    let entered = Arc::new(Notify::new());
    let server = tokio::spawn(
        tonic::transport::Server::builder()
            .add_service(native_wire::carrier_server::CarrierServer::new(
                HungCarrier {
                    entered: entered.clone(),
                },
            ))
            .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(listener)),
    );
    let observed = Arc::new(Mutex::new(Vec::new()));
    let record = observed.clone();
    let route = TestingTcpRoute::register_with_kind(
        source.identity.instance_id.to_string(),
        move |kind, peer, canonical| {
            record.lock().unwrap().push((kind, peer, canonical));
            let destination = destination.clone();
            Box::pin(async move { Ok(destination) })
        },
    )
    .unwrap();
    let (intent, _) = source.build_intent(&target, &single).await;
    let handle = source.handle();
    let mut build = tokio::spawn(async move { handle.execute_public_v1(intent).await });
    tokio::time::timeout(Duration::from_secs(10), entered.notified())
        .await
        .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(60), &mut build)
            .await
            .is_err()
    );
    assert_eq!(observed.lock().unwrap()[0].0, TestingTcpRouteKind::Build);
    let close = source.intent(PublicOperationProgram::Close).await;
    let close_id = close.operation_id.clone();
    tokio::time::timeout(
        Duration::from_secs(10),
        source.handle().execute_public_v1(close),
    )
    .await
    .expect("terminal containment did not drain a hung native Build")
    .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_secs(10), build)
            .await
            .unwrap()
            .unwrap()
            .is_err()
    );
    assert_eq!(
        source
            .handle()
            .public_v1_raw_retained_task_cardinality()
            .unwrap(),
        (0, 0)
    );
    let connection =
        rusqlite::Connection::open(SqliteStore::metadata_database_path(&source.root)).unwrap();
    let json: String = connection
        .query_row(
            "SELECT state_json FROM agent_state WHERE singleton = 1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let state: serde_json::Value = serde_json::from_str(&json).unwrap();
    let close = &state["publicV1OperationState"]["operations"][close_id.as_str()];
    assert_eq!(close["stage"], "completed");
    assert_eq!(close["disposition"]["kind"], "succeeded");
    assert!(
        close["lifecycle"]["outcomes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|outcome| {
                outcome["CloseFailure"]["child"] == "Replicator"
                    && outcome["CloseFailure"]["error"]
                        .as_str()
                        .is_some_and(|error| !error.is_empty())
            }),
        "native child Close failure was not retained after outer containment"
    );
    assert_ne!(source.report().await.write_status, AccessStatus::Granted);
    drop(route);
    server.abort();
    let _ = server.await;
    source.stop().await;
    target.stop().await;
}

#[tokio::test]
async fn public_v1_open_owns_one_registration_listener_and_writer_and_close_failure_is_retained() {
    let root = root();
    let resource = ResourceUid::new("native-open-once");
    let mut node = Node::start(root.path(), &resource, 1).await;
    assert_eq!(node.service.opens.load(Ordering::SeqCst), 0);
    assert_eq!(node.handle().public_v1_provider_writer_count(), 0);
    let open = node
        .intent(PublicOperationProgram::Open {
            replica: node.identity.clone(),
            existing: node.report().await.open_mode == OpenMode::Existing,
        })
        .await;
    node.handle().execute_public_v1(open.clone()).await.unwrap();
    node.handle().execute_public_v1(open).await.unwrap();
    assert_eq!(node.service.opens.load(Ordering::SeqCst), 1);
    assert_eq!(node.service.factories.load(Ordering::SeqCst), 1);
    assert_eq!(node.handle().public_v1_provider_writer_count(), 1);
    let partition = node.service.partition.read().await.clone().unwrap();
    assert!(
        partition
            .with_factory(Arc::new(DefaultReplicatorFactory::new()))
            .create_replicator(
                Some(node.service.provider.clone()),
                Some(ReplicatorSettings {
                    replication_address: node.service.address.clone()
                })
            )
            .await
            .is_err()
    );
    assert_eq!(node.service.factories.load(Ordering::SeqCst), 1);
    let address: tonic::codegen::http::Uri = node
        .report()
        .await
        .replication_address
        .unwrap()
        .parse()
        .unwrap();
    let socket = address.authority().unwrap().as_str().to_owned();
    assert!(std::net::TcpListener::bind(&socket).is_err());
    node.service.close_failure.store(true, Ordering::Release);
    let intent = node.intent(PublicOperationProgram::Close).await;
    let operation = intent.operation_id.clone();
    node.handle().execute_public_v1(intent).await.unwrap();
    assert_eq!(node.handle().public_v1_provider_writer_count(), 0);
    assert!(std::net::TcpListener::bind(socket).is_ok());
    let store = SqliteStore::open_public_v1(
        SqliteStore::metadata_database_path(&node.root),
        &node.preview,
    )
    .unwrap();
    let record = store
        .public_v1_operation_snapshot(&operation)
        .await
        .unwrap();
    assert_eq!(
        record.disposition,
        Some(
            kuberic_runtime::testing::sqlite_store::PublicV1OperationSnapshotDisposition::Succeeded
        )
    );
    let state = close_journal(&node, &operation);
    assert!(
        state["lifecycle"]["outcomes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|outcome| {
                outcome["CloseFailure"]["child"] == "Application"
                    && outcome["CloseFailure"]["error"]
                        .as_str()
                        .is_some_and(|error| error.contains("injected application close failure"))
            })
    );
    node.stop().await;
}

fn close_journal(node: &Node, operation: &OperationId) -> serde_json::Value {
    let connection =
        rusqlite::Connection::open(SqliteStore::metadata_database_path(&node.root)).unwrap();
    let json: String = connection
        .query_row(
            "SELECT state_json FROM agent_state WHERE singleton = 1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let state: serde_json::Value = serde_json::from_str(&json).unwrap();
    state["publicV1OperationState"]["operations"][operation.as_str()].clone()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hosted_sf06_failed_replicator_close_retains_diagnostic_and_joins_native_provider() {
    let root = root();
    let mut node = Node::start(root.path(), &ResourceUid::new("close-native-join"), 1).await;
    node.open(ReplicaRole::Primary, EPOCH).await;
    node.single(EPOCH).await;
    let (release, blocked) = std::sync::mpsc::channel();
    *node.service.provider.synchronous_epoch.lock().unwrap() = Some(blocked);
    let control = node.service.control.read().await.clone().unwrap();
    let epoch = tokio::spawn(async move { control.update_epoch(Epoch::new(0, 2)).await });
    node.service.provider.epoch_entered.notified().await;
    node.service
        .provider
        .close_failure
        .store(true, Ordering::Release);
    let intent = node.intent(PublicOperationProgram::Close).await;
    let operation = intent.operation_id.clone();
    let handle = node.handle();
    let replay = intent.clone();
    let mut close = tokio::spawn(async move { handle.execute_public_v1(intent).await });
    eventually(|| node.service.aborts.load(Ordering::SeqCst) == 1).await;
    tokio::time::timeout(Duration::from_secs(5), async {
        while close_journal(&node, &operation)["lifecycle"]["in_flight"] != "Cleanup" {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let pending = close_journal(&node, &operation);
    assert_eq!(pending["stage"], "running");
    assert_eq!(pending["lifecycle"]["in_flight"], "Cleanup");
    assert!(
        pending["lifecycle"]["outcomes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|outcome| {
                outcome["CloseFailure"]["child"] == "Replicator"
                    && outcome["CloseFailure"]["error"]
                        .as_str()
                        .is_some_and(|error| error.contains("injected replicator close failure"))
            })
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(40), &mut close)
            .await
            .is_err()
    );
    assert!(!epoch.is_finished());
    assert_eq!(node.service.closes.load(Ordering::SeqCst), 0);
    assert_eq!(node.handle().public_v1_provider_writer_count(), 1);
    release.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(10), close)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let _ = epoch.await.unwrap();
    assert_eq!(node.handle().public_v1_provider_writer_count(), 0);
    assert_eq!(
        node.handle()
            .public_v1_raw_retained_task_cardinality()
            .unwrap(),
        (0, 0)
    );
    let record = close_journal(&node, &operation);
    assert_eq!(record["stage"], "completed");
    assert_eq!(record["disposition"]["kind"], "succeeded");
    let calls = node.service.observed.lock().unwrap().len();
    node.handle().execute_public_v1(replay).await.unwrap();
    assert_eq!(node.service.observed.lock().unwrap().len(), calls);
    assert_eq!(node.service.aborts.load(Ordering::SeqCst), 1);
    node.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hosted_sf06_failed_application_close_joins_service_work_despite_dropped_observer() {
    let root = root();
    let mut node = Node::start(root.path(), &ResourceUid::new("close-application-join"), 1).await;
    node.open(ReplicaRole::Primary, EPOCH).await;
    node.single(EPOCH).await;
    let entered = Arc::new(Notify::new());
    let joined = entered.clone();
    let (release, blocked) = std::sync::mpsc::channel();
    let task = tokio::spawn(async move {
        joined.notify_one();
        blocked.recv_timeout(Duration::from_secs(15)).unwrap();
        std::future::pending::<()>().await;
    });
    let retained = task.abort_handle();
    node.service.tasks.lock().unwrap().push(task);
    entered.notified().await;
    node.service.close_failure.store(true, Ordering::Release);
    let intent = node.intent(PublicOperationProgram::Close).await;
    let replay = intent.clone();
    let operation = intent.operation_id.clone();
    let handle = node.handle();
    let mut observer = tokio::spawn(async move { handle.execute_public_v1(intent).await });
    eventually(|| node.service.closes.load(Ordering::SeqCst) == 1).await;
    assert!(
        tokio::time::timeout(Duration::from_millis(40), &mut observer)
            .await
            .is_err()
    );
    assert!(!retained.is_finished());
    assert_eq!(
        close_journal(&node, &operation)["lifecycle"]["in_flight"],
        "ApplicationClose"
    );
    observer.abort();
    assert!(observer.await.unwrap_err().is_cancelled());
    release.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        while close_journal(&node, &operation)["stage"] != "completed" {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(retained.is_finished());
    let record = close_journal(&node, &operation);
    assert_eq!(record["disposition"]["kind"], "succeeded");
    assert!(
        record["lifecycle"]["outcomes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|outcome| {
                outcome["CloseFailure"]["child"] == "Application"
                    && outcome["CloseFailure"]["error"]
                        .as_str()
                        .is_some_and(|error| error.contains("injected application close failure"))
            })
    );
    assert_eq!(node.service.closes.load(Ordering::SeqCst), 1);
    assert_eq!(node.service.aborts.load(Ordering::SeqCst), 1);
    assert_eq!(node.handle().public_v1_provider_writer_count(), 0);
    assert_eq!(
        node.handle()
            .public_v1_raw_retained_task_cardinality()
            .unwrap(),
        (0, 0)
    );
    node.handle().execute_public_v1(replay).await.unwrap();
    assert_eq!(node.service.closes.load(Ordering::SeqCst), 1);
    node.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hosted_sf06_interrupted_child_close_is_joined_cancelled_and_never_repeated() {
    let root = root();
    for replicator in [false, true] {
        let mut node = Node::start(
            &root.path().join(format!("replicator-{replicator}")),
            &ResourceUid::new("close-interrupted-callback"),
            1,
        )
        .await;
        node.open(ReplicaRole::Primary, EPOCH).await;
        node.single(EPOCH).await;
        let (release, blocked) = std::sync::mpsc::channel();
        if replicator {
            *node.service.provider.synchronous_close.lock().unwrap() = Some(blocked);
        } else {
            *node.service.synchronous_close.lock().unwrap() = Some(blocked);
        }
        let intent = node.intent(PublicOperationProgram::Close).await;
        let operation = intent.operation_id.clone();
        let replay = intent.clone();
        let handle = node.handle();
        let observer = tokio::spawn(async move { handle.execute_public_v1(intent).await });
        if replicator {
            node.service.provider.close_entered.notified().await;
        } else {
            node.service.close_entered.notified().await;
        }
        let abort = node.intent(PublicOperationProgram::Abort).await;
        let successor = abort.operation_id.clone();
        let handle = node.handle();
        let mut terminal = tokio::spawn(async move { handle.execute_public_v1(abort).await });
        tokio::time::timeout(Duration::from_secs(5), async {
            while close_journal(&node, &successor)["stage"] != "waitingForContainment" {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(40), &mut terminal)
                .await
                .is_err()
        );
        assert_eq!(close_journal(&node, &operation)["stage"], "running");
        release.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(10), terminal)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(observer.await.unwrap().is_err());
        let record = close_journal(&node, &operation);
        assert_eq!(record["stage"], "completed");
        assert_eq!(record["disposition"]["kind"], "cancelled");
        let calls = node.service.observed.lock().unwrap().len();
        let application_calls = node.service.closes.load(Ordering::SeqCst);
        assert!(node.handle().execute_public_v1(replay).await.is_err());
        assert_eq!(node.service.observed.lock().unwrap().len(), calls);
        assert_eq!(
            node.service.closes.load(Ordering::SeqCst),
            application_calls
        );
        assert_eq!(application_calls, u64::from(!replicator));
        assert_eq!(node.service.aborts.load(Ordering::SeqCst), 1);
        assert_eq!(node.handle().public_v1_provider_writer_count(), 0);
        assert_eq!(
            node.handle()
                .public_v1_raw_retained_task_cardinality()
                .unwrap(),
            (0, 0)
        );
        node.stop().await;
    }
}

fn seed_close_recovery(
    node: &Node,
    intent: &PublicOperationIntent,
    child: &str,
    cleanup_recorded: bool,
    already_pending: bool,
) {
    let mut connection =
        rusqlite::Connection::open(SqliteStore::metadata_database_path(&node.root)).unwrap();
    connection.busy_timeout(Duration::from_secs(5)).unwrap();
    let transaction = connection
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .unwrap();
    let json: String = transaction
        .query_row(
            "SELECT state_json FROM agent_state WHERE singleton = 1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let mut state: serde_json::Value = serde_json::from_str(&json).unwrap();
    let preview = &mut state["publicV1OperationState"];
    let outcomes = if cleanup_recorded {
        serde_json::json!(["Done", {"CloseFailure": {
            "child": "Replicator", "error": "retained receipt close diagnostic"
        }}, "Done", "Done"])
    } else if child == "ApplicationClose" {
        serde_json::json!(["Done", "Done"])
    } else {
        serde_json::json!(["Done"])
    };
    let record = serde_json::json!({
        "intent": intent,
        "acceptedHistory": preview["history"],
        "acceptedStorage": preview["storageIncarnation"],
        "stage": if already_pending { "containmentPending" } else { "running" },
        "disposition": if already_pending {
            serde_json::json!({"kind": "ambiguous", "detail": "seeded lost close owner"})
        } else { serde_json::Value::Null },
        "containment": if already_pending { "pending" } else { "notRequired" },
        "supersededBy": null,
        "blockers": [],
        "lifecycle": {
            "in_flight": if cleanup_recorded { serde_json::Value::Null } else { serde_json::json!(child) },
            "outcomes": outcomes
        }
    });
    preview["operations"][intent.operation_id.as_str()] = record;
    preview["currentOperation"] = serde_json::json!(intent.operation_id);
    preview["terminal"] = serde_json::json!(true);
    preview["writesRevoked"] = serde_json::json!(true);
    transaction
        .execute(
            "UPDATE agent_state SET state_json = ?1 WHERE singleton = 1",
            [serde_json::to_string(&state).unwrap()],
        )
        .unwrap();
    transaction.commit().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hosted_sf06_recovery_attaches_real_native_join_without_dispatch_or_cleanup_receipt_trust()
{
    let root = root();
    for (child, cleanup_recorded, predecessor, already_pending) in [
        ("ReplicatorClose", false, false, false),
        ("ApplicationClose", false, false, true),
        ("ApplicationClose", false, true, false),
        ("ReplicatorClose", true, false, false),
    ] {
        let mut node = Node::start(
            &root
                .path()
                .join(format!("{child}-{cleanup_recorded}-{predecessor}")),
            &ResourceUid::new("close-recovery-native-owner"),
            1,
        )
        .await;
        node.open(ReplicaRole::Primary, EPOCH).await;
        node.single(EPOCH).await;
        let (release, blocked) = std::sync::mpsc::channel();
        *node.service.provider.synchronous_epoch.lock().unwrap() = Some(blocked);
        let control = node.service.control.read().await.clone().unwrap();
        let epoch = tokio::spawn(async move { control.update_epoch(Epoch::new(0, 2)).await });
        node.service.provider.epoch_entered.notified().await;
        let mut intent = node.intent(PublicOperationProgram::Close).await;
        if predecessor {
            intent.process_session_id = ProcessSessionId::new("predecessor-session");
        }
        let operation = intent.operation_id.clone();
        // This is a durable recovery fixture, not a claim that this process
        // executed the seeded callback. All containment below is the real host.
        seed_close_recovery(&node, &intent, child, cleanup_recorded, already_pending);
        let replay = if cleanup_recorded {
            let handle = node.handle();
            let intent = intent.clone();
            Some(tokio::spawn(async move {
                handle.execute_public_v1(intent).await
            }))
        } else {
            None
        };
        eventually(|| node.service.aborts.load(Ordering::SeqCst) == 1).await;
        let pending = close_journal(&node, &operation);
        assert_eq!(
            pending["stage"],
            if cleanup_recorded {
                "running"
            } else {
                "containmentPending"
            }
        );
        assert!(!epoch.is_finished());
        assert_eq!(node.handle().public_v1_provider_writer_count(), 1);
        assert_eq!(node.service.closes.load(Ordering::SeqCst), 0);
        assert!(
            !node
                .service
                .observed
                .lock()
                .unwrap()
                .iter()
                .any(|call| matches!(call, NativeCall::Close))
        );
        assert_ne!(node.report().await.write_status, AccessStatus::Granted);
        release.send(()).unwrap();
        let _ = epoch.await.unwrap();
        if let Some(replay) = replay {
            tokio::time::timeout(Duration::from_secs(10), replay)
                .await
                .unwrap()
                .unwrap()
                .unwrap();
        }
        tokio::time::timeout(Duration::from_secs(10), async {
            while close_journal(&node, &operation)["stage"] != "completed" {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let record = close_journal(&node, &operation);
        assert_eq!(
            record["disposition"]["kind"],
            if cleanup_recorded {
                "succeeded"
            } else {
                "ambiguous"
            }
        );
        if cleanup_recorded {
            assert_eq!(
                record["lifecycle"]["outcomes"][1]["CloseFailure"]["error"],
                "retained receipt close diagnostic"
            );
        } else {
            assert!(node.handle().execute_public_v1(intent).await.is_err());
        }
        assert_eq!(node.handle().public_v1_provider_writer_count(), 0);
        assert_eq!(
            node.handle()
                .public_v1_raw_retained_task_cardinality()
                .unwrap(),
            (0, 0)
        );
        assert_eq!(node.service.aborts.load(Ordering::SeqCst), 1);
        assert_eq!(node.service.closes.load(Ordering::SeqCst), 0);
        node.stop().await;
    }
}

#[tokio::test]
async fn retained_service_state_reopens_without_replaying_interrupted_application_children() {
    let root = root();
    let resource = ResourceUid::new("native-restart");
    let mut node = Node::start(root.path(), &resource, 1).await;
    node.open(ReplicaRole::Primary, EPOCH).await;
    node.single(EPOCH).await;
    node.service
        .write(Bytes::from_static(b"retained"))
        .await
        .unwrap();
    let prior = node.report().await;
    node.stop().await;
    let mut restarted = Node::start(root.path(), &resource, 1).await;
    assert_ne!(
        restarted.handle().process_session_id(),
        prior.process_session_id
    );
    assert_eq!(restarted.service.opens.load(Ordering::SeqCst), 0);
    assert_eq!(
        restarted.service.provider.values(),
        [Bytes::from_static(b"retained")]
    );
    restarted.open(ReplicaRole::Primary, Epoch::new(0, 2)).await;
    restarted.single(Epoch::new(0, 2)).await;
    assert_eq!(
        restarted.service.values().await.unwrap(),
        [Bytes::from_static(b"retained")]
    );
    assert_eq!(
        restarted
            .service
            .write(Bytes::from_static(b"after-restart"))
            .await
            .unwrap(),
        2
    );
    restarted.stop().await;
}

#[tokio::test]
async fn incompatible_or_corrupt_provider_and_agent_formats_reject_before_mutation() {
    let root = root();
    let application = root.path().join("application");
    std::fs::create_dir_all(&application).unwrap();
    let bad = application.join("service.json");
    std::fs::write(&bad, br#"{"format":"old-engine","epoch":{},"values":[]}"#).unwrap();
    assert!(Provider::new(application.clone()).is_err());
    assert_eq!(
        std::fs::read(&bad).unwrap(),
        br#"{"format":"old-engine","epoch":{},"values":[]}"#
    );
    let resource = ResourceUid::new("old-agent-schema");
    let mut node = Node::start(&root.path().join("agent"), &resource, 1).await;
    node.stop().await;
    let database = SqliteStore::metadata_database_path(&node.root);
    let connection = rusqlite::Connection::open(&database).unwrap();
    connection.pragma_update(None, "user_version", 6).unwrap();
    drop(connection);
    let before = std::fs::read(&database).unwrap();
    assert!(SqliteStore::open_public_v1(&database, &node.preview).is_err());
    assert_eq!(std::fs::read(database).unwrap(), before);
}

#[tokio::test]
async fn retained_secondary_promotion_preserves_acknowledged_bytes_and_replaces_service_location() {
    let root = root();
    let pair = Pair::start(root.path(), 1).await;
    pair.source
        .service
        .write(Bytes::from_static(b"before-promotion"))
        .await
        .unwrap();
    assert!(pair.source.report().await.service_location.is_some());
    let epoch = Epoch::new(0, 2);
    pair.source
        .command(PublicOperationProgram::Role {
            epoch,
            role: ReplicaRole::ActiveSecondary,
        })
        .await
        .unwrap();
    pair.source
        .command(PublicOperationProgram::Epoch { epoch })
        .await
        .unwrap();
    pair.target
        .command(PublicOperationProgram::Role {
            epoch,
            role: ReplicaRole::Primary,
        })
        .await
        .unwrap();
    pair.target
        .command(PublicOperationProgram::Epoch { epoch })
        .await
        .unwrap();
    let promoted = ConfigurationDescriptor::new(
        epoch,
        pair.target.identity.replica_id,
        vec![
            ConfigurationMember {
                identity: pair.target.identity.clone(),
                role: ReplicaRole::Primary,
            },
            ConfigurationMember {
                identity: pair.source.identity.clone(),
                role: ReplicaRole::ActiveSecondary,
            },
        ],
        2,
    );
    for (local, remote, role) in [
        (&pair.source, &pair.target, ReplicaRole::Primary),
        (&pair.target, &pair.source, ReplicaRole::ActiveSecondary),
    ] {
        let configuration = PublicConfiguration {
            current: promoted.clone(),
            previous: None,
            replicas: vec![remote.descriptor(role).await],
        };
        local
            .command(if local.identity == pair.target.identity {
                PublicOperationProgram::CatchUp {
                    configuration,
                    mode: PublicCatchUpMode::All,
                }
            } else {
                PublicOperationProgram::Configuration(configuration)
            })
            .await
            .unwrap();
    }
    assert!(pair.source.report().await.service_location.is_none());
    assert!(pair.target.report().await.service_location.is_some());
    assert!(
        pair.source
            .service
            .write(Bytes::from_static(b"revoked-primary"))
            .await
            .is_err()
    );
    assert_eq!(pair.target.service.values().await.unwrap().len(), 2);
    assert_eq!(
        pair.target
            .service
            .write(Bytes::from_static(b"after-promotion"))
            .await
            .unwrap(),
        3
    );
    assert_eq!(
        pair.source.service.provider.values(),
        pair.target.service.provider.values()
    );
    pair.stop().await;
}

#[tokio::test]
async fn exact_idle_removal_does_not_reauthorize_retired_service_work() {
    let root = root();
    let resource = ResourceUid::new("native-idle-removal");
    let mut source = Node::start(&root.path().join("source"), &resource, 1).await;
    let mut target = Node::start(&root.path().join("target"), &resource, 2).await;
    source.open(ReplicaRole::Primary, EPOCH).await;
    let single = source.single(EPOCH).await;
    source
        .service
        .write(Bytes::from_static(b"before-removal"))
        .await
        .unwrap();
    target.open(ReplicaRole::IdleSecondary, EPOCH).await;
    let build = source.build(&target, &single).await;
    target.command(PublicOperationProgram::Abort).await.unwrap();
    let removal = source
        .intent(PublicOperationProgram::Remove(PublicBuildInput {
            attempt: build.attempt.build_id.clone(),
            epoch: build.attempt.epoch,
            current_configuration_id: Some(build.current_configuration_id.clone()),
            replica: build.attempt.target.clone(),
            target_storage: build.attempt.target_storage.clone(),
            process_session_id: build.attempt.target_session.clone(),
            replication_address: build.target_address.clone(),
            history: Some(build.history.clone()),
            source_address: build.source_address.clone(),
        }))
        .await;
    source
        .handle()
        .execute_public_v1(removal.clone())
        .await
        .unwrap();
    source
        .handle()
        .execute_public_v1(removal.clone())
        .await
        .unwrap();
    let report = source.report().await;
    assert!(report.completed_operations.iter().any(|completion| {
        completion.operation_id == removal.operation_id
            && completion.call_kind == PublicV1CallKind::Remove
            && completion.result == PublicV1OperationResult::Succeeded
    }));
    assert_eq!(
        source
            .service
            .write(Bytes::from_static(b"after-removal"))
            .await
            .unwrap(),
        2
    );
    assert_eq!(target.service.provider.values().len(), 1);
    assert!(
        target
            .service
            .write(Bytes::from_static(b"retired"))
            .await
            .is_err()
    );
    source.stop().await;
    target.stop().await;
}

#[tokio::test]
async fn terminal_close_restarts_fenced_without_replaying_application_open() {
    let root = root();
    let resource = ResourceUid::new("terminal-restart");
    let mut node = Node::start(root.path(), &resource, 1).await;
    node.open(ReplicaRole::Primary, EPOCH).await;
    node.single(EPOCH).await;
    node.service
        .write(Bytes::from_static(b"durable-before-terminal"))
        .await
        .unwrap();
    node.command(PublicOperationProgram::Close).await.unwrap();
    node.stop().await;
    let mut restarted = Node::start(root.path(), &resource, 1).await;
    assert_eq!(restarted.service.opens.load(Ordering::SeqCst), 0);
    assert_eq!(
        restarted.service.provider.values(),
        [Bytes::from_static(b"durable-before-terminal")]
    );
    assert_ne!(restarted.report().await.write_status, AccessStatus::Granted);
    assert!(
        restarted
            .command(PublicOperationProgram::Open {
                replica: restarted.identity.clone(),
                existing: true,
            })
            .await
            .is_err()
    );
    assert_eq!(restarted.service.opens.load(Ordering::SeqCst), 0);
    restarted.stop().await;
}

#[tokio::test]
async fn caller_owned_control_listener_is_released_when_startup_is_cancelled() {
    let root = root();
    for drop_sender in [false, true] {
        let resource = ResourceUid::new(format!("cancel-startup-{drop_sender}"));
        let service = Application::new(
            root.path().join(format!("service-{drop_sender}")),
            "http://127.0.0.1:0".into(),
        )
        .unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let data_root = root.path().join(format!("host-{drop_sender}"));
        let host = ReplicaHost::new(
            ReplicaProcessConfig {
                resource_uid: resource.clone(),
                replica_id: ReplicaId::new(1),
                pod_uid: PodUid::new("cancel-pod"),
                pvc_uid: PvcUid::new("cancel-pvc"),
                data_root: data_root.clone(),
                replication_address: "127.0.0.1:0".parse().unwrap(),
                bearer_token: TOKEN.into(),
                rpc_deadline: Duration::from_secs(2),
                transport_window_capacity: 8,
            },
            service.clone(),
            ApplicationStorageState::FreshEmpty,
            Arc::new(Resolver),
        )
        .with_public_v1_binding(PublicV1LifecycleBinding {
            identity: PublicV1Identity::new(1),
            resource_uid: resource,
            spec_generation: 1,
            state_persistence: StatePersistence::Persisted,
        });
        let (shutdown, receiver) = watch::channel(false);
        let startup = tokio::spawn(host.start_with_shutdown(listener, receiver));
        if drop_sender {
            drop(shutdown);
        } else {
            shutdown.send_replace(true);
        }
        assert!(
            tokio::time::timeout(Duration::from_secs(10), startup)
                .await
                .unwrap()
                .unwrap()
                .unwrap()
                .is_none()
        );
        tokio::net::TcpListener::bind(address).await.unwrap();
        assert!(!SqliteStore::metadata_database_path(&data_root).exists());
        assert_eq!(service.opens.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test]
async fn replacement_initialization_persists_exact_identity_before_application_open() {
    use kuberic_runtime::control::proto;
    let root = root();
    let resource = ResourceUid::new("native-public-initialization");
    let service =
        Application::new(root.path().join("application"), "http://127.0.0.1:0".into()).unwrap();
    *service.owner.lock().unwrap() = Arc::downgrade(&service);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let preview = PublicV1Identity::new(1);
    let host = ReplicaHost::new(
        ReplicaProcessConfig {
            resource_uid: resource.clone(),
            replica_id: ReplicaId::new(1),
            pod_uid: PodUid::new("fresh-pod"),
            pvc_uid: PvcUid::new("fresh-pvc"),
            data_root: root.path().to_path_buf(),
            replication_address: "127.0.0.1:0".parse().unwrap(),
            bearer_token: TOKEN.into(),
            rpc_deadline: Duration::from_secs(5),
            transport_window_capacity: 8,
        },
        service.clone(),
        ApplicationStorageState::FreshEmpty,
        Arc::new(Resolver),
    )
    .with_public_v1_binding(PublicV1LifecycleBinding {
        identity: preview.clone(),
        resource_uid: resource.clone(),
        spec_generation: 1,
        state_persistence: StatePersistence::Persisted,
    })
    .with_application_storage_paths(std::collections::BTreeMap::from([(
        "application".into(),
        root.path().join("application"),
    )]));
    let startup = tokio::spawn(host.start(listener));
    let mut client = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match proto::agent_control_client::AgentControlClient::connect(format!(
                "http://{address}"
            ))
            .await
            {
                Ok(client) => break client,
                Err(_) => tokio::time::sleep(Duration::from_millis(10)).await,
            }
        }
    })
    .await
    .unwrap();
    let authorized = |message| {
        let mut request = Request::new(message);
        request
            .metadata_mut()
            .insert("authorization", format!("Bearer {TOKEN}").parse().unwrap());
        request
    };
    let mut request = Request::new(proto::GetAgentStatusRequest {
        protocol_version: kuberic_runtime::protocol::PROTOCOL_VERSION,
        resource_uid: resource.to_string(),
        replica_id: 1,
        expected_instance_id: "fresh-pod".into(),
    });
    request
        .metadata_mut()
        .insert("authorization", format!("Bearer {TOKEN}").parse().unwrap());
    let status = client.get_status(request).await.unwrap().into_inner();
    assert_eq!(
        status.storage_state,
        proto::AgentStorageState::Uninitialized as i32
    );
    assert_eq!(service.opens.load(Ordering::SeqCst), 0);
    let initialization = derive_initialization_id(
        &resource,
        ReplicaId::new(1),
        &PodUid::new("fresh-pod"),
        &PvcUid::new("fresh-pvc"),
    );
    let identity = ReplicaIdentity {
        replica_id: ReplicaId::new(1),
        instance_id: ReplicaInstanceId::new("fresh-pod"),
        agent_generation: derive_agent_generation(&initialization),
    };
    let configuration = ConfigurationDescriptor::new(
        EPOCH,
        identity.replica_id,
        vec![ConfigurationMember {
            identity: identity.clone(),
            role: ReplicaRole::Primary,
        }],
        1,
    );
    client
        .execute(authorized(proto::ExecuteCommandRequest {
            protocol_version: kuberic_runtime::protocol::PROTOCOL_VERSION,
            resource_uid: resource.to_string(),
            target: Some(identity.clone().into()),
            expected_process_session_id: status.process_session_id,
            command: Some(
                proto::execute_command_request::Command::InitializeAgentStore(
                    proto::InitializeAgentStoreCommand {
                        initialization_id: initialization.to_string(),
                        resource_uid: resource.to_string(),
                        local_replica_id: 1,
                        expected_instance_id: "fresh-pod".into(),
                        expected_pod_uid: "fresh-pod".into(),
                        expected_pvc_uid: "fresh-pvc".into(),
                        assigned_agent_generation: identity.agent_generation.to_string(),
                        effective_policy: Some(EffectivePolicy::fixed(1, 1).unwrap().into()),
                        bootstrap_configuration: Some(configuration.into()),
                        provisioning: None,
                    },
                ),
            ),
        }))
        .await
        .unwrap();
    let mut running = tokio::time::timeout(Duration::from_secs(10), startup)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let report = running.handle().public_v1_report().await.unwrap();
    assert_eq!(report.replica, identity);
    assert_eq!(report.open_mode, OpenMode::New);
    assert_eq!(service.opens.load(Ordering::SeqCst), 0);
    let database = SqliteStore::metadata_database_path(root.path());
    let connection = rusqlite::Connection::open(&database).unwrap();
    let version: u32 = connection
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap();
    assert_eq!(
        version,
        kuberic_runtime::testing::sqlite_store::PUBLIC_V1_REPLACEMENT_SCHEMA_VERSION
    );
    drop(connection);
    let program = PublicOperationProgram::Open {
        replica: identity,
        existing: false,
    };
    running
        .handle()
        .execute_public_v1(PublicOperationIntent {
            identity: preview,
            operation_id: OperationId::new("public-initialization-open"),
            revision: 1,
            process_session_id: report.process_session_id,
            class: PublicOperationClass::Authority,
            input_digest: program.digest(),
            replay_disposition: program.replay_disposition(),
            lifecycle: None,
            program: Some(program),
        })
        .await
        .unwrap();
    assert_eq!(service.opens.load(Ordering::SeqCst), 1);
    running.shutdown();
    running.wait().await.unwrap();
    service.drain().await;
    drop(client);
    tokio::net::TcpListener::bind(address).await.unwrap();
}

#[tokio::test]
async fn production_config_has_no_protocol_selector_or_application_commit_oracle() {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let process = std::fs::read_to_string(manifest.join("src/host/process.rs")).unwrap();
    let config = process
        .split("pub struct ReplicaProcessConfig")
        .nth(1)
        .unwrap()
        .split('}')
        .next()
        .unwrap();
    assert!(!config.contains("preview"));
    assert!(!manifest.join("src/replicator/stateless/mod.rs").exists());
    let replicator = std::fs::read_to_string(manifest.join("src/replicator/mod.rs")).unwrap();
    assert!(!replicator.contains("mod stateless;"));
    let production =
        std::fs::read_to_string(manifest.join("src/replicator/v1/service_adapter/production.rs"))
            .unwrap();
    for forbidden in [
        "ManagedReplicaStore",
        "DurableState",
        "LocalWriteJournal",
        "ReplicationProgressStore",
    ] {
        assert!(
            !production.contains(forbidden),
            "native owner depends on service persistence: {forbidden}"
        );
    }
}

#[tokio::test]
async fn native_same_role_secondary_epoch_fences_predecessor_binding_and_sending_epoch() {
    let root = root();
    let mut pair = Pair::start(root.path(), 1).await;
    let before = pair.target.report().await;
    let predecessor = binding(&pair).await;
    let predecessor_frame = replication_frame(&pair, 2, Bytes::from_static(b"predecessor")).await;
    pair.target.service.observed.lock().unwrap().clear();
    let mut operations = pair.target.service.control_operations();
    let epoch = Epoch::new(0, 2);
    pair.target
        .command(PublicOperationProgram::Epoch { epoch })
        .await
        .unwrap();
    pair.source
        .command(PublicOperationProgram::Epoch { epoch })
        .await
        .unwrap();
    let current = ConfigurationDescriptor::new(
        epoch,
        pair.source.identity.replica_id,
        vec![
            ConfigurationMember {
                identity: pair.source.identity.clone(),
                role: ReplicaRole::Primary,
            },
            ConfigurationMember {
                identity: pair.target.identity.clone(),
                role: ReplicaRole::ActiveSecondary,
            },
        ],
        2,
    );
    for (local, remote, role) in [
        (&pair.target, &pair.source, ReplicaRole::Primary),
        (&pair.source, &pair.target, ReplicaRole::ActiveSecondary),
    ] {
        local
            .command(PublicOperationProgram::Configuration(PublicConfiguration {
                current: current.clone(),
                previous: None,
                replicas: vec![remote.descriptor(role).await],
            }))
            .await
            .unwrap();
    }
    pair.configuration = current;
    let observed = pair.target.service.observed.lock().unwrap().clone();
    assert!(
        observed
            .iter()
            .any(|call| matches!(call, NativeCall::Epoch(selected) if *selected == epoch))
    );
    assert!(
        !observed
            .iter()
            .any(|call| matches!(call, NativeCall::Role(..))),
        "same-role epoch advance recreated the native role/receiver"
    );
    let after = pair.target.report().await;
    assert_eq!(after.role, ReplicaRole::ActiveSecondary);
    assert_eq!(after.replicator_creation_id, before.replicator_creation_id);
    assert_eq!(after.replication_address, before.replication_address);
    assert_eq!(after.storage_incarnation, before.storage_incarnation);
    let address = after.replication_address.unwrap();
    let error = exchange(
        address.clone(),
        &predecessor,
        &format!("Bearer {TOKEN}"),
        vec![predecessor_frame],
    )
    .await
    .unwrap_err();
    assert_eq!(error.code(), tonic::Code::PermissionDenied);
    let mut stale = replication_frame(&pair, 2, Bytes::from_static(b"old-sending-epoch")).await;
    let Some(native_wire::envelope::Body::ReplicationOperation(body)) = &mut stale.body else {
        unreachable!()
    };
    body.sending_epoch = Some(native_wire::Epoch {
        data_loss: 0,
        configuration: 1,
    });
    let stale_stream = exchange(
        address,
        &binding(&pair).await,
        &format!("Bearer {TOKEN}"),
        vec![stale],
    )
    .await
    .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(80), operations.recv())
            .await
            .is_err()
    );
    assert_eq!(
        pair.target.service.provider.values(),
        [Bytes::from_static(b"value-1")]
    );
    assert_eq!(pair.target.report().await.progress.current(), Some(1));
    drop(stale_stream);
    let mut historical = replication_frame(&pair, 1, Bytes::from_static(b"value-1")).await;
    let Some(native_wire::envelope::Body::ReplicationOperation(body)) = &mut historical.body else {
        unreachable!()
    };
    body.original_epoch = Some(native_wire::Epoch {
        data_loss: 0,
        configuration: 1,
    });
    let mut historical_stream = exchange(
        pair.target.report().await.replication_address.unwrap(),
        &binding(&pair).await,
        &format!("Bearer {TOKEN}"),
        vec![historical],
    )
    .await
    .unwrap();
    applied(&mut historical_stream, 1).await;
    assert!(
        operations.try_recv().is_err(),
        "legitimate historical-original retry was redelivered"
    );
    assert_eq!(
        pair.target.service.provider.values(),
        [Bytes::from_static(b"value-1")]
    );
    drop(historical_stream);
    let write = pair
        .source
        .service
        .begin_write(Bytes::from_static(b"current-epoch"))
        .await
        .unwrap();
    let operation = delivered(&mut operations).await;
    assert_eq!((operation.lsn, operation.epoch), (2, epoch));
    pair.target
        .service
        .provider
        .persist_received(&operation)
        .await
        .unwrap();
    operation.acknowledge().unwrap();
    assert_eq!(write.wait().await.unwrap(), 2);
    pair.stop().await;
}

#[tokio::test]
async fn native_build_callback_receives_invalid_progress_and_exact_frozen_idle_target() {
    let root = root();
    let resource = ResourceUid::new("observed-native-build");
    let mut source = Node::start(&root.path().join("source"), &resource, 1).await;
    let mut target = Node::start(&root.path().join("target"), &resource, 2).await;
    source.open(ReplicaRole::Primary, EPOCH).await;
    let single = source.single(EPOCH).await;
    target.open(ReplicaRole::IdleSecondary, EPOCH).await;
    target.service.provider.install.hold();
    let (intent, authority) = source.build_intent(&target, &single).await;
    let expected = intent.clone();
    let handle = source.handle();
    let mut build = tokio::spawn(async move { handle.execute_public_v1(intent).await });
    tokio::time::timeout(
        Duration::from_secs(10),
        target.service.provider.install.entered.notified(),
    )
    .await
    .unwrap();
    let observed = source.service.observed.lock().unwrap().clone();
    let info = observed
        .iter()
        .find_map(|call| match call {
            NativeCall::Build(info) => Some(info),
            _ => None,
        })
        .unwrap();
    assert_eq!(
        info.current_progress,
        kuberic_runtime::application::INVALID_LSN
    );
    assert_eq!(
        info.catch_up_capability,
        kuberic_runtime::application::INVALID_LSN
    );
    assert_eq!(info.role, ReplicaRole::IdleSecondary);
    assert!(!info.must_catchup);
    assert_eq!(info.build_id, authority.attempt.build_id);
    assert_eq!(info.identity, target.identity);
    assert_eq!(info.storage_incarnation.as_ref(), Some(&target.storage));
    assert_eq!(
        info.process_session_id,
        target.handle().process_session_id()
    );
    assert_eq!(info.replication_address, authority.target_address);
    let role_index = observed
        .iter()
        .position(
            |call| matches!(call, NativeCall::Role(epoch, ReplicaRole::Primary) if *epoch == EPOCH),
        )
        .unwrap();
    let configuration_index = observed
        .iter()
        .position(|call| matches!(call, NativeCall::Current(_)))
        .unwrap();
    assert!(
        role_index < configuration_index,
        "native primary configuration preceded completed Primary role"
    );
    for call in &observed {
        if let NativeCall::Current(configuration) = call {
            assert!(
                !configuration
                    .configuration
                    .members
                    .iter()
                    .any(|member| member.identity == target.identity)
            );
            assert!(configuration.replicas.iter().all(|peer| {
                peer.identity != source.identity && peer.role == ReplicaRole::ActiveSecondary
            }));
        }
    }
    assert!(
        tokio::time::timeout(Duration::from_millis(40), &mut build)
            .await
            .is_err()
    );
    assert!(
        !source
            .report()
            .await
            .completed_operations
            .iter()
            .any(|completion| completion.operation_id == expected.operation_id)
    );
    target.service.provider.install.release();
    tokio::time::timeout(Duration::from_secs(10), build)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let report = source.report().await;
    let completion = report
        .completed_operations
        .iter()
        .find(|completion| completion.operation_id == expected.operation_id)
        .unwrap();
    assert_eq!(completion.revision, expected.revision);
    assert_eq!(completion.process_session_id, expected.process_session_id);
    assert_eq!(completion.input_digest, expected.input_digest);
    assert_eq!(completion.replay_disposition, expected.replay_disposition);
    assert_eq!(completion.history, authority.history);
    assert_eq!(completion.storage_incarnation, source.storage);
    assert_eq!(
        completion.current_configuration_id.as_ref(),
        Some(&authority.current_configuration_id)
    );
    assert_eq!(completion.call_kind, PublicV1CallKind::Build);
    assert_eq!(completion.result, PublicV1OperationResult::Succeeded);
    source.stop().await;
    target.stop().await;
}

#[tokio::test]
async fn native_configuration_dispatch_rejects_self_idle_and_nonmember_descriptors() {
    let root = root();
    let pair = Pair::start(root.path(), 0).await;
    let valid = PublicConfiguration {
        current: pair.configuration.clone(),
        previous: None,
        replicas: vec![pair.target.descriptor(ReplicaRole::ActiveSecondary).await],
    };
    for invalid in 0..3 {
        let mut input = valid.clone();
        match invalid {
            0 => input
                .replicas
                .push(pair.source.descriptor(ReplicaRole::Primary).await),
            1 => input.replicas[0].role = ReplicaRole::IdleSecondary,
            _ => input.replicas[0].identity.replica_id = ReplicaId::new(99),
        }
        let count = pair.source.service.observed.lock().unwrap().len();
        assert!(
            pair.source
                .command(PublicOperationProgram::Configuration(input))
                .await
                .is_err()
        );
        assert_eq!(pair.source.service.observed.lock().unwrap().len(), count);
    }
    pair.source
        .command(PublicOperationProgram::Configuration(valid))
        .await
        .unwrap();
    for call in pair.source.service.observed.lock().unwrap().iter() {
        if let NativeCall::Current(configuration) = call {
            assert!(
                configuration
                    .replicas
                    .iter()
                    .all(|peer| peer.identity != pair.source.identity
                        && peer.role == ReplicaRole::ActiveSecondary)
            );
        }
    }
    pair.stop().await;
}

#[tokio::test]
async fn native_remove_cancels_session_without_late_install_and_close_drains_retained_provider() {
    let root = root();
    let resource = ResourceUid::new("remove-running-native-build");
    let mut source = Node::start(&root.path().join("source"), &resource, 1).await;
    let mut target = Node::start(&root.path().join("target"), &resource, 2).await;
    source.open(ReplicaRole::Primary, EPOCH).await;
    let single = source.single(EPOCH).await;
    source
        .service
        .write(Bytes::from_static(b"committed-before-build"))
        .await
        .unwrap();
    target.open(ReplicaRole::IdleSecondary, EPOCH).await;
    source.service.provider.copy.hold();
    let (intent, authority) = source.build_intent(&target, &single).await;
    let cancelled_intent = intent.clone();
    let build_id = intent.operation_id.clone();
    let handle = source.handle();
    let build = tokio::spawn(async move { handle.execute_public_v1(intent).await });
    tokio::time::timeout(
        Duration::from_secs(10),
        source.service.provider.copy.entered.notified(),
    )
    .await
    .unwrap();
    assert_eq!(
        source.service.provider.copy_active.load(Ordering::Acquire),
        1
    );
    let removal = source
        .intent(PublicOperationProgram::Remove(PublicBuildInput {
            attempt: authority.attempt.build_id.clone(),
            epoch: authority.attempt.epoch,
            current_configuration_id: Some(authority.current_configuration_id.clone()),
            replica: target.identity.clone(),
            target_storage: target.storage.clone(),
            process_session_id: target.handle().process_session_id(),
            replication_address: authority.target_address.clone(),
            history: Some(authority.history.clone()),
            source_address: authority.source_address.clone(),
        }))
        .await;
    let removal_id = removal.operation_id.clone();
    let handle = source.handle();
    let mut remove = tokio::spawn(async move { handle.execute_public_v1(removal).await });
    let result = tokio::time::timeout(Duration::from_secs(10), &mut remove).await;
    if result.is_err() {
        source.service.provider.copy.release();
        let _ = tokio::time::timeout(
            Duration::from_secs(10),
            source.command(PublicOperationProgram::Abort),
        )
        .await;
        let _ = remove.await;
        let _ = build.await;
        panic!("public Remove did not cancel its active native Build");
    }
    result.unwrap().unwrap().unwrap();
    assert!(
        tokio::time::timeout(Duration::from_secs(10), build)
            .await
            .unwrap()
            .unwrap()
            .is_err()
    );
    assert_eq!(
        source.service.provider.copy_active.load(Ordering::SeqCst),
        1,
        "native Remove must retain the in-flight provider call for separate drainage",
    );
    assert_eq!(
        source
            .service
            .provider
            .copy_interrupted
            .load(Ordering::SeqCst),
        0
    );
    let calls = source.service.observed.lock().unwrap().clone();
    let removed = calls.iter().position(|call| matches!(call,
        NativeCall::RemoveBegin(id, active) if *id == target.identity.replica_id && *active == 1,
    )).expect("protected native Remove did not overlap the held Build callback");
    assert!(
        calls[removed + 1..].iter().any(|call| matches!(call,
            NativeCall::RemoveEnd(id, active) if *id == target.identity.replica_id && *active == 1,
        )),
        "protected Remove did not preserve the actual provider callback owner"
    );
    let report = source.report().await;
    assert!(
        report
            .completed_operations
            .iter()
            .any(|completion| completion.operation_id == build_id
                && completion.result == PublicV1OperationResult::Cancelled)
    );
    assert!(
        report
            .completed_operations
            .iter()
            .any(|completion| completion.operation_id == removal_id
                && completion.result == PublicV1OperationResult::Succeeded)
    );
    let installed = report.configuration.as_ref().unwrap();
    assert_eq!(installed.voting.current, single);
    assert!(
        !report
            .engine_idle
            .as_ref()
            .unwrap()
            .idle_replicas
            .contains(&target.identity.replica_id)
    );
    assert_eq!(
        source.service.provider.copy_active.load(Ordering::SeqCst),
        1
    );
    assert!(target.service.provider.values().is_empty());
    source.service.provider.copy.release();
    tokio::time::timeout(Duration::from_secs(10), async {
        while source.service.provider.copy_active.load(Ordering::SeqCst) != 0 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("retained provider callback did not drain after release");
    assert_eq!(
        source
            .service
            .provider
            .copy_interrupted
            .load(Ordering::SeqCst),
        0,
        "Remove abandoned the provider future instead of retaining its actual return"
    );
    assert!(
        source
            .handle()
            .execute_public_v1(cancelled_intent)
            .await
            .is_err(),
        "replaying the cancelled Build resurrected the removed session"
    );
    assert_eq!(
        source
            .service
            .observed
            .lock()
            .unwrap()
            .iter()
            .filter(|call| matches!(call, NativeCall::Build(_)))
            .count(),
        1
    );
    tokio::time::sleep(Duration::from_millis(60)).await;
    assert!(target.service.provider.values().is_empty());
    assert_eq!(target.service.provider.installs.load(Ordering::SeqCst), 0);
    assert!(
        !single
            .members
            .iter()
            .any(|member| member.identity == target.identity)
    );
    let report = source.report().await;
    assert!(report.completed_operations.iter().any(|completion| {
        completion.operation_id == build_id
            && completion.result == PublicV1OperationResult::Cancelled
    }));
    assert!(
        !report
            .engine_idle
            .as_ref()
            .unwrap()
            .idle_replicas
            .contains(&target.identity.replica_id)
    );
    assert_eq!(report.role, ReplicaRole::Primary);
    assert_eq!(
        source.service.provider.values(),
        [Bytes::from_static(b"committed-before-build")]
    );
    tokio::time::timeout(
        Duration::from_secs(10),
        source.command(PublicOperationProgram::Close),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(
        source
            .handle()
            .public_v1_raw_retained_task_cardinality()
            .unwrap(),
        (0, 0)
    );
    assert_eq!(
        source.service.provider.copy_active.load(Ordering::SeqCst),
        0
    );
    assert!(target.service.provider.values().is_empty());
    assert_eq!(target.service.provider.installs.load(Ordering::SeqCst), 0);
    source.stop().await;
    target.stop().await;
}

#[tokio::test]
async fn native_replication_body_epochs_cannot_escape_a_valid_admitted_binding() {
    let root = root();
    for crossed in 0..4 {
        let pair = Pair::start(&root.path().join(crossed.to_string()), 0).await;
        let mut operations = pair.target.service.control_operations();
        let before = pair.target.report().await;
        let callbacks = pair.target.service.provider.callback_counts();
        let state = std::fs::read(pair.target.service.provider.root.join("service.json")).unwrap();
        let mut frame = replication_frame(&pair, 1, Bytes::from_static(b"unauthorized-body")).await;
        let Some(native_wire::envelope::Body::ReplicationOperation(body)) = &mut frame.body else {
            unreachable!()
        };
        match crossed {
            0 => body.sending_epoch.as_mut().unwrap().configuration = 2,
            1 => body.sending_epoch.as_mut().unwrap().data_loss = 1,
            2 => body.original_epoch.as_mut().unwrap().data_loss = 1,
            _ => body.original_epoch.as_mut().unwrap().configuration = 2,
        }
        let mut response = exchange(
            before.replication_address.clone().unwrap(),
            &binding(&pair).await,
            &format!("Bearer {TOKEN}"),
            vec![frame],
        )
        .await
        .unwrap();
        no_applied_ack(&mut response, OperationKind::Replication, 1).await;
        assert!(operations.try_recv().is_err());
        let after = pair.target.report().await;
        assert_eq!(after.progress, before.progress);
        assert_eq!(after.completed_operations, before.completed_operations);
        assert_eq!(pair.target.service.provider.callback_counts(), callbacks);
        assert_eq!(
            std::fs::read(pair.target.service.provider.root.join("service.json")).unwrap(),
            state
        );
        assert!(pair.source.service.provider.values().is_empty());
        drop(response);
        pair.stop().await;
    }
}

#[tokio::test]
async fn native_build_start_and_copy_body_epochs_are_checked_before_receiver_mutation() {
    let root = root();
    for (copy, future_loss) in [(false, false), (false, true), (true, false), (true, true)] {
        let directory = root.path().join(format!("copy-{copy}-loss-{future_loss}"));
        let resource = ResourceUid::new(format!("body-epoch-{copy}-{future_loss}"));
        let mut source = Node::start(&directory.join("source"), &resource, 1).await;
        let mut target = Node::start(&directory.join("target"), &resource, 2).await;
        source.open(ReplicaRole::Primary, EPOCH).await;
        let single = source.single(EPOCH).await;
        target.open(ReplicaRole::IdleSecondary, EPOCH).await;
        let (_intent, authority) = source.build_intent(&target, &single).await;
        let binding = build_binding(&source, &target, &authority).await;
        let address = target.report().await.replication_address.unwrap();
        let mut existing = None;
        if copy {
            let start = native_frame(
                &source,
                &target,
                native_wire::envelope::Body::StartCopy(native_wire::StartCopy {
                    epoch: Some(native_wire::Epoch {
                        data_loss: 0,
                        configuration: 1,
                    }),
                    replica: target.identity.replica_id.value(),
                    replication_start: 1,
                }),
            )
            .await;
            let mut response = exchange(
                address.clone(),
                &binding,
                &format!("Bearer {TOKEN}"),
                vec![start],
            )
            .await
            .unwrap();
            tokio::time::timeout(Duration::from_secs(10), async {
                loop {
                    let envelope = response.message().await.unwrap().unwrap();
                    if matches!(
                        envelope.body,
                        Some(native_wire::envelope::Body::CopyContextOperation(_))
                    ) {
                        break;
                    }
                }
            })
            .await
            .unwrap();
            existing = Some(response);
        }
        let mut operations = target.service.control_operations();
        let before = target.report().await;
        let callbacks = target.service.provider.callback_counts();
        let state = std::fs::read(target.service.provider.root.join("service.json")).unwrap();
        let epoch = native_wire::Epoch {
            data_loss: if future_loss { 1 } else { 0 },
            configuration: if future_loss { 1 } else { 2 },
        };
        let body = if copy {
            native_wire::envelope::Body::CopyOperation(native_wire::CopyOperation {
                metadata: Some(native_wire::Metadata {
                    sequence: 0,
                    operation_type: 1,
                    atomic_group: -1,
                }),
                epoch: Some(epoch),
                buffers: Some(native_wire::Buffers {
                    segments: vec![Bytes::from(
                        serde_json::to_vec(&ServiceState::default()).unwrap(),
                    )],
                }),
                replica: target.identity.replica_id.value(),
                is_last: true,
            })
        } else {
            native_wire::envelope::Body::StartCopy(native_wire::StartCopy {
                epoch: Some(epoch),
                replica: target.identity.replica_id.value(),
                replication_start: 1,
            })
        };
        let frame = native_frame(&source, &target, body).await;
        let mut response = exchange(address, &binding, &format!("Bearer {TOKEN}"), vec![frame])
            .await
            .unwrap();
        no_applied_ack(&mut response, OperationKind::Copy, 0).await;
        assert!(operations.try_recv().is_err());
        let after = target.report().await;
        assert_eq!(after.progress, before.progress);
        assert_eq!(after.completed_operations, before.completed_operations);
        assert_eq!(target.service.provider.callback_counts(), callbacks);
        assert_eq!(
            std::fs::read(target.service.provider.root.join("service.json")).unwrap(),
            state
        );
        drop(existing);
        drop(response);
        source.stop().await;
        target.stop().await;
    }
}

#[tokio::test]
async fn native_build_binding_pins_frozen_source_address_even_when_source_is_unreachable() {
    let root = root();
    let resource = ResourceUid::new("frozen-source-build-address");
    let mut source = Node::start(&root.path().join("source"), &resource, 1).await;
    let mut target = Node::start(&root.path().join("target"), &resource, 2).await;
    source.open(ReplicaRole::Primary, EPOCH).await;
    let single = source.single(EPOCH).await;
    target.open(ReplicaRole::IdleSecondary, EPOCH).await;
    let (intent, authority) = source.build_intent(&target, &single).await;
    let valid = build_binding(&source, &target, &authority).await;
    assert_eq!(valid["primary"]["address"], authority.source_address);
    let encoded = serde_json::to_vec(&authority).unwrap();
    assert_eq!(
        serde_json::from_slice::<PublicInboundBuildAuthority>(&encoded).unwrap(),
        authority
    );
    let mut changed_intent = intent.clone();
    if let Some(PublicOperationProgram::Build(input)) = &mut changed_intent.program {
        input.source_address = "http://127.0.0.1:9/".into();
    }
    assert_ne!(
        changed_intent.program.as_ref().unwrap().digest(),
        intent.input_digest
    );
    assert!(changed_intent.validate().is_err());
    let callbacks = target.service.provider.callback_counts();
    let before = target.report().await;
    let bytes = std::fs::read(target.service.provider.root.join("service.json")).unwrap();
    source.stop().await;
    let mut changed = valid.clone();
    changed["primary"]["address"] = "http://127.0.0.1:9/".into();
    let address = before.replication_address.clone().unwrap();
    let error = exchange(
        address.clone(),
        &changed,
        &format!("Bearer {TOKEN}"),
        Vec::new(),
    )
    .await
    .unwrap_err();
    assert_eq!(error.code(), tonic::Code::PermissionDenied);
    assert_eq!(target.service.provider.callback_counts(), callbacks);
    assert_eq!(target.report().await.progress, before.progress);
    assert_eq!(
        std::fs::read(target.service.provider.root.join("service.json")).unwrap(),
        bytes
    );
    let accepted = exchange(address, &valid, &format!("Bearer {TOKEN}"), Vec::new())
        .await
        .unwrap();
    drop(accepted);
    target.stop().await;
}

#[tokio::test]
async fn native_swap_freezes_specific_quorum_and_conservative_all_fallback_inputs() {
    let root = root();
    for specific in [false, true] {
        let directory = root.path().join(specific.to_string());
        let resource = ResourceUid::new(format!("captured-native-swap-{specific}"));
        let mut source = Node::start(&directory.join("source"), &resource, 1).await;
        let mut target = Node::start(&directory.join("target"), &resource, 2).await;
        source
            .service
            .provider
            .conservative_quorum
            .store(!specific, Ordering::Release);
        source.open(ReplicaRole::Primary, EPOCH).await;
        let single = source.single(EPOCH).await;
        source
            .service
            .write(Bytes::from_static(b"before-swap"))
            .await
            .unwrap();
        target.open(ReplicaRole::IdleSecondary, EPOCH).await;
        source.build(&target, &single).await;
        target
            .command(PublicOperationProgram::Role {
                epoch: EPOCH,
                role: ReplicaRole::ActiveSecondary,
            })
            .await
            .unwrap();
        target
            .command(PublicOperationProgram::Epoch { epoch: EPOCH })
            .await
            .unwrap();
        let current = ConfigurationDescriptor::new(
            EPOCH,
            source.identity.replica_id,
            vec![
                ConfigurationMember {
                    identity: source.identity.clone(),
                    role: ReplicaRole::Primary,
                },
                ConfigurationMember {
                    identity: target.identity.clone(),
                    role: ReplicaRole::ActiveSecondary,
                },
            ],
            2,
        );
        for (local, remote, role) in [
            (&target, &source, ReplicaRole::Primary),
            (&source, &target, ReplicaRole::ActiveSecondary),
        ] {
            local
                .command(PublicOperationProgram::Configuration(PublicConfiguration {
                    current: current.clone(),
                    previous: None,
                    replicas: vec![remote.descriptor(role).await],
                }))
                .await
                .unwrap();
        }
        let report = source.report().await;
        assert_eq!(report.extensions.catch_up_specific_quorum, Some(specific));
        source
            .service
            .provider
            .probe_fenced_admission
            .store(true, Ordering::Release);
        let mut candidate = target.descriptor(ReplicaRole::ActiveSecondary).await;
        candidate.must_catchup = true;
        let starting = PublicConfiguration {
            current: current.clone(),
            previous: None,
            replicas: vec![candidate.clone()],
        };
        let epoch = Epoch::new(0, 2);
        let refreshed = PublicConfiguration {
            current: ConfigurationDescriptor::new(
                epoch,
                source.identity.replica_id,
                current.members.clone(),
                2,
            ),
            previous: None,
            replicas: vec![candidate],
        };
        target
            .command(PublicOperationProgram::Epoch { epoch })
            .await
            .unwrap();
        target
            .command(PublicOperationProgram::Configuration(PublicConfiguration {
                current: refreshed.current.clone(),
                previous: None,
                replicas: vec![source.descriptor(ReplicaRole::Primary).await],
            }))
            .await
            .unwrap();
        source.service.observed.lock().unwrap().clear();
        let mode = if specific {
            PublicCatchUpMode::WriteQuorum
        } else {
            PublicCatchUpMode::All
        };
        let program = PublicOperationProgram::Swap {
            starting,
            refreshed: refreshed.clone(),
            epoch,
            handoff: ReplicaRole::ActiveSecondary,
            catch_up_specific_quorum: specific,
            mode,
            controller_composed: false,
        };
        let frozen = source.intent(program).await;
        let mut crossed = frozen.clone();
        crossed.operation_id = OperationId::new(format!("crossed-swap-{specific}"));
        if let Some(PublicOperationProgram::Swap {
            catch_up_specific_quorum,
            mode,
            ..
        }) = &mut crossed.program
        {
            *catch_up_specific_quorum = !specific;
            *mode = if specific {
                PublicCatchUpMode::All
            } else {
                PublicCatchUpMode::WriteQuorum
            };
        }
        crossed.input_digest = crossed.program.as_ref().unwrap().digest();
        assert!(source.handle().execute_public_v1(crossed).await.is_err());
        assert!(
            source.service.observed.lock().unwrap().is_empty(),
            "substituted capability dispatched native work"
        );
        if let Err(error) = source.handle().execute_public_v1(frozen.clone()).await {
            let connection =
                rusqlite::Connection::open(SqliteStore::metadata_database_path(&source.root))
                    .unwrap();
            let json: String = connection
                .query_row(
                    "SELECT state_json FROM agent_state WHERE singleton=1",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            let state: serde_json::Value = serde_json::from_str(&json).unwrap();
            panic!(
                "native Swap failed (specific={specific}): {error}; record={}; calls={:?}",
                state["publicV1OperationState"]["operations"][frozen.operation_id.as_str()],
                source.service.observed.lock().unwrap()
            );
        }
        let observed = source.service.observed.lock().unwrap().clone();
        let waits = observed
            .iter()
            .filter_map(|call| match call {
                NativeCall::Wait(mode, status) => Some((*mode, *status)),
                _ => None,
            })
            .collect::<Vec<_>>();
        let native_mode = if specific {
            ReplicaSetQuorumMode::WriteQuorum
        } else {
            ReplicaSetQuorumMode::All
        };
        assert_eq!(waits.len(), 2);
        assert_eq!(waits[0], (native_mode, AccessStatus::Granted));
        assert_eq!(waits[1].0, native_mode);
        assert_ne!(waits[1].1, AccessStatus::Granted);
        let raw_rejections = observed
            .iter()
            .filter_map(|call| match call {
                NativeCall::FencedBegin(status, error) => Some((*status, error)),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(raw_rejections.len(), 1);
        assert_ne!(raw_rejections[0].0, AccessStatus::Granted);
        assert!(!raw_rejections[0].1.is_empty());
        let configurations = observed
            .iter()
            .filter_map(|call| match call {
                NativeCall::CatchUp(current, previous) => Some((current, previous)),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            configurations.len(),
            2,
            "both Swap installs must use native catch-up configuration, not final current config"
        );
        assert_eq!(configurations[0].0.configuration.epoch, EPOCH);
        assert_eq!(configurations[1].0.configuration.epoch, epoch);
        for (current, previous) in configurations {
            assert!(
                current
                    .replicas
                    .iter()
                    .all(|peer| peer.identity != source.identity
                        && peer.role == ReplicaRole::ActiveSecondary
                        && peer.must_catchup)
            );
            assert!(
                previous
                    .replicas
                    .iter()
                    .all(|peer| peer.identity != source.identity)
            );
        }
        let completed = source.report().await;
        let receipt = completed
            .completed_operations
            .iter()
            .find(|receipt| receipt.operation_id == frozen.operation_id)
            .unwrap();
        assert_eq!(receipt.input_digest, frozen.input_digest);
        assert_eq!(receipt.process_session_id, frozen.process_session_id);
        assert_eq!(receipt.revision, frozen.revision);
        assert_eq!(receipt.catch_up_specific_quorum, Some(specific));
        assert_eq!(receipt.catch_up_mode, Some(mode));
        assert_eq!(
            receipt.current_configuration_id.as_ref(),
            Some(&refreshed.current.configuration_id)
        );
        assert_eq!(receipt.result, PublicV1OperationResult::Succeeded);
        let store = SqliteStore::open_public_v1(
            SqliteStore::metadata_database_path(&source.root),
            &source.preview,
        )
        .unwrap();
        assert_eq!(
            store
                .public_v1_operation_snapshot(&frozen.operation_id)
                .await
                .unwrap()
                .intent,
            frozen
        );
        source
            .handle()
            .execute_public_v1(frozen.clone())
            .await
            .unwrap();
        assert_eq!(
            source.service.observed.lock().unwrap().len(),
            observed.len(),
            "exact Swap replay reselected its mode"
        );
        assert!(
            source
                .service
                .write(Bytes::from_static(b"old-primary"))
                .await
                .is_err()
        );
        assert_eq!(
            target.service.provider.values(),
            [Bytes::from_static(b"before-swap")]
        );
        source.stop().await;
        target.stop().await;
    }
}

#[tokio::test]
async fn native_controller_unreachable_survivor_fences_until_exact_return_or_explicit_retirement() {
    use kuberic_runtime::control::proto;
    let root = root();
    for retire in [false, true] {
        let directory = root.path().join(retire.to_string());
        let mut pair = Pair::start(&directory, 1).await;
        let predecessor = pair
            .source
            .handle()
            .public_v1_process_endpoint(ReplicaRole::Primary)
            .await
            .unwrap();
        let survivor = pair
            .target
            .handle()
            .public_v1_process_endpoint(ReplicaRole::ActiveSecondary)
            .await
            .unwrap();
        let resource = pair.source.history.resource_uid.clone();
        pair.source.stop().await;
        let mut replacement = Node::start(&directory.join("source"), &resource, 1).await;
        replacement
            .command(PublicOperationProgram::Open {
                replica: replacement.identity.clone(),
                existing: true,
            })
            .await
            .unwrap();
        let endpoint = replacement
            .handle()
            .public_v1_process_endpoint(ReplicaRole::None)
            .await
            .unwrap();
        let epoch = Epoch::new(0, 2);
        let required = ProcessSessionRenewalRequired {
            operation_id: OperationId::new(format!("unreachable-native-survivor-{retire}")),
            reason: ProcessSessionRenewalReason::PrimaryReplacement,
            resource_uid: resource.clone(),
            history: replacement.history.clone(),
            previous_epoch: EPOCH,
            renewal_epoch: epoch,
            predecessor,
            replacement: Some(endpoint.clone()),
            surviving_sessions: vec![survivor.clone()],
            fresh_configuration: ConfigurationDescriptor::new(
                epoch,
                replacement.identity.replica_id,
                if retire {
                    vec![ConfigurationMember {
                        identity: replacement.identity.clone(),
                        role: ReplicaRole::Primary,
                    }]
                } else {
                    vec![
                        ConfigurationMember {
                            identity: replacement.identity.clone(),
                            role: ReplicaRole::Primary,
                        },
                        ConfigurationMember {
                            identity: pair.target.identity.clone(),
                            role: ReplicaRole::ActiveSecondary,
                        },
                    ]
                },
                if retire { 1 } else { 2 },
            ),
            predecessor_contained: true,
            data_loss: None,
        };
        required.validate().unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let unavailable_address = format!("http://{}", listener.local_addr().unwrap());
        let blackhole = tokio::spawn(async move {
            loop {
                let (socket, _) = listener.accept().await.unwrap();
                drop(socket);
            }
        });
        let mut unavailable =
            proto::agent_control_client::AgentControlClient::connect(unavailable_address.clone())
                .await
                .unwrap();
        let mut probe = Request::new(proto::GetAgentStatusRequest {
            protocol_version: kuberic_runtime::protocol::PROTOCOL_VERSION,
            resource_uid: resource.to_string(),
            replica_id: survivor.identity.replica_id.value(),
            expected_instance_id: survivor.identity.instance_id.to_string(),
        });
        probe
            .metadata_mut()
            .insert("authorization", format!("Bearer {TOKEN}").parse().unwrap());
        probe.set_timeout(Duration::from_secs(1));
        assert!(
            unavailable.get_status(probe).await.is_err(),
            "blackholed actual control RPC succeeded"
        );
        let mut controller = RenewalController {
            status: Mutex::new(None),
            replacement: replacement.handle(),
            endpoints: vec![
                (endpoint, replacement.control_address.clone()),
                (survivor.clone(), unavailable_address),
            ],
        };
        let scope = PublicV1Scope {
            namespace: "testing".into(),
            name: "survivor-policy".into(),
            resource_uid: resource,
        };
        let initial = PublicV1SessionRenewalStatus::required(required).unwrap();
        controller
            .persist_public_v1_session_renewal(&scope, None, &initial)
            .await
            .unwrap();
        let blocked = loop {
            let current = controller
                .observe_public_v1_session_renewal(&scope)
                .await
                .unwrap()
                .unwrap();
            match plan_public_v1_session_renewal(
                &current,
                std::slice::from_ref(&survivor.process_session_id),
            )
            .unwrap()
            {
                PublicV1SessionRenewalPlan::WaitForSurvivor {
                    survivor: captured, ..
                } => {
                    assert_eq!(*captured, survivor);
                    break current;
                }
                plan => execute_public_v1_session_renewal(&controller, &scope, plan)
                    .await
                    .unwrap(),
            }
        };
        for _ in 0..3 {
            assert!(matches!(
                plan_public_v1_session_renewal(
                    &blocked,
                    std::slice::from_ref(&survivor.process_session_id),
                )
                .unwrap(),
                PublicV1SessionRenewalPlan::WaitForSurvivor { .. }
            ));
            let report = wire_report(&replacement).await;
            assert_ne!(report.read_status, AccessStatus::Granted);
            assert_ne!(report.write_status, AccessStatus::Granted);
            assert!(
                replacement
                    .service
                    .write(Bytes::from_static(b"cannot-skip-survivor"))
                    .await
                    .is_err()
            );
        }
        assert!(blocked.finish_epoch_barriers().is_err());
        assert!(
            !replacement
                .service
                .observed
                .lock()
                .unwrap()
                .iter()
                .any(|call| matches!(call, NativeCall::Role(..)))
        );
        if retire {
            pair.target.stop().await;
            let next = accept_public_v1_survivor_retired(
                &blocked,
                &survivor,
                OperationId::new("explicit-exact-survivor-retirement"),
            )
            .unwrap();
            controller
                .persist_public_v1_session_renewal(&scope, Some(&blocked), &next)
                .await
                .unwrap();
        } else {
            controller.endpoints[1].1 = pair.target.control_address.clone();
        }
        blackhole.abort();
        let _ = blackhole.await;
        let mut stable = false;
        for _ in 0..24 {
            let current = controller
                .observe_public_v1_session_renewal(&scope)
                .await
                .unwrap()
                .unwrap();
            let plan = plan_public_v1_session_renewal(&current, &[]).unwrap();
            if matches!(plan, PublicV1SessionRenewalPlan::Stable) {
                assert_eq!(current.stage, PublicSessionRenewalStage::Authorized);
                stable = true;
                break;
            }
            if let PublicV1SessionRenewalPlan::Dispatch {
                command: ProtocolCommand::PublicV1History(history),
                ..
            } = &plan
                && let PublicV1HistoryCommand::SessionRenewal { command } = history.as_ref()
                && let PublicV1SessionRenewalCommand::InstallFreshConfiguration {
                    configuration,
                    ..
                } = command.as_ref()
            {
                if !retire {
                    pair.target
                        .command(PublicOperationProgram::Configuration(PublicConfiguration {
                            current: configuration.clone(),
                            previous: None,
                            replicas: vec![replacement.descriptor(ReplicaRole::Primary).await],
                        }))
                        .await
                        .unwrap();
                }
                replacement
                    .command(PublicOperationProgram::Configuration(PublicConfiguration {
                        current: configuration.clone(),
                        previous: None,
                        replicas: if retire {
                            Vec::new()
                        } else {
                            vec![pair.target.descriptor(ReplicaRole::ActiveSecondary).await]
                        },
                    }))
                    .await
                    .unwrap();
            }
            execute_public_v1_session_renewal(&controller, &scope, plan)
                .await
                .unwrap();
        }
        assert!(
            stable,
            "controller renewal did not resolve a returned/retired exact survivor"
        );
        assert_eq!(
            replacement
                .service
                .write(Bytes::from_static(b"after-renewal"))
                .await
                .unwrap(),
            2
        );
        assert_eq!(replacement.service.provider.values().len(), 2);
        if !retire {
            assert_eq!(
                pair.target.service.provider.values(),
                replacement.service.provider.values()
            );
        }
        replacement.stop().await;
        if !retire {
            pair.target.stop().await;
        }
    }
}

#[tokio::test]
async fn native_query_outcomes_remain_independent_through_host_wire_and_controller_eligibility() {
    use kuberic_controller::evaluator::public_v1_topology::{
        PublicV1TopologyPlan, PublicV1TopologyReplica, plan_public_v1_topology,
    };
    let root = root();
    let resource = ResourceUid::new("connected-native-query-outcomes");
    let mut first = Node::start(root.path(), &resource, 1).await;
    first.open(ReplicaRole::Primary, EPOCH).await;
    first.single(EPOCH).await;
    first
        .service
        .write(Bytes::from_static(b"genuinely-committed"))
        .await
        .unwrap();
    first.stop().await;
    let mut node = Node::start(root.path(), &resource, 1).await;
    node.command(PublicOperationProgram::Open {
        replica: node.identity.clone(),
        existing: true,
    })
    .await
    .unwrap();
    let baseline = wire_report(&node).await;
    assert_eq!(baseline.progress.current(), Some(1));
    assert_eq!(baseline.progress.capability(), Some(0));
    let accepted = PublicV1AcceptedStatus {
        identity: node.preview.clone(),
        history_initialization_id: derive_history_initialization_id(
            &resource,
            PublicProtocolGeneration::new(1),
        ),
        history: node.history.clone(),
        data_loss_recovery: None,
        completed_operations: Vec::new(),
        extensions: Default::default(),
    };
    for (current, capability) in [(0, 0), (1, 0), (2, 0), (0, 1), (0, 2), (1, 2), (2, 1)] {
        node.service
            .provider
            .query_current
            .store(current, Ordering::Release);
        node.service
            .provider
            .query_capability
            .store(capability, Ordering::Release);
        let host = node.report().await;
        let report = wire_report(&node).await;
        assert_eq!(report.progress, host.progress);
        assert_eq!(report.history, baseline.history);
        assert_eq!(report.storage_incarnation, baseline.storage_incarnation);
        assert_eq!(report.read_status, AccessStatus::NotPrimary);
        assert_eq!(report.write_status, AccessStatus::NotPrimary);
        match current {
            0 => assert_eq!(
                report.progress.current_progress,
                ProgressQueryResult::Value(1)
            ),
            1 => assert_eq!(
                report.progress.current_progress,
                ProgressQueryResult::NotReady
            ),
            _ => assert!(
                matches!(&report.progress.current_progress, ProgressQueryResult::Failed(error)
                if error.contains("current-progress query failure"))
            ),
        }
        match capability {
            0 => assert_eq!(
                report.progress.catch_up_capability,
                ProgressQueryResult::Value(0)
            ),
            1 => assert_eq!(
                report.progress.catch_up_capability,
                ProgressQueryResult::NotReady
            ),
            _ => assert!(
                matches!(&report.progress.catch_up_capability, ProgressQueryResult::Failed(error)
                if error.contains("catch-up-capability query failure"))
            ),
        }
        let plan = plan_public_v1_topology(
            &accepted,
            None,
            &[PublicV1TopologyReplica {
                replication_address: report.replication_address.clone().unwrap(),
                report,
            }],
            1,
            None,
            None,
            "query-matrix",
        )
        .unwrap();
        if current != 0 {
            assert!(
                matches!(plan, PublicV1TopologyPlan::Wait),
                "unknown/failed current progress became an eligible primary: {plan:?}"
            );
        } else {
            assert!(
                !matches!(plan, PublicV1TopologyPlan::Wait),
                "independent capability outcome erased known current progress"
            );
        }
    }
    node.service
        .provider
        .query_current
        .store(0, Ordering::Release);
    node.service
        .provider
        .query_capability
        .store(0, Ordering::Release);
    assert_eq!(
        node.service.provider.values(),
        [Bytes::from_static(b"genuinely-committed")]
    );
    node.stop().await;
}

#[tokio::test]
async fn hosted_public_v1_authorized_data_loss_preserves_effective_outcomes_and_history_scope() {
    let root = root();
    for outcome in 0..4 {
        let resource = ResourceUid::new(format!("native-data-loss-{outcome}"));
        let mut node = Node::start(&root.path().join(outcome.to_string()), &resource, 1).await;
        node.command(PublicOperationProgram::Open {
            replica: node.identity.clone(),
            existing: node.report().await.open_mode == OpenMode::Existing,
        })
        .await
        .unwrap();
        let before = node.report().await;
        let authorization_id = OperationId::new(format!("native-data-loss-{outcome}"));
        let proposed =
            derive_data_loss_history(&authorization_id, &node.history, 1, &node.storage).unwrap();
        let authorization = DataLossAuthorization {
            authorization_id: authorization_id.clone(),
            predecessor: node.history.clone(),
            proposed: proposed.clone(),
            provisional_primary: node.storage.clone(),
            reset_scope: if matches!(outcome, 1 | 3) {
                ProviderProgressReset::ResetToEmpty
            } else {
                ProviderProgressReset::Preserve
            },
            replay_disposition: DataLossReplay::ReplaceOnAmbiguity,
        };
        let endpoint = node
            .handle()
            .public_v1_process_endpoint(ReplicaRole::None)
            .await
            .unwrap();
        let epoch = Epoch::new(1, 1);
        let attempt = DataLossAttempt {
            authorization_id,
            attempt_id: OperationId::new(format!("native-recovery-{outcome}")),
            process: ProcessAttemptContext {
                process_session_id: endpoint.process_session_id,
                replicator_creation_id: endpoint.replicator_creation_id,
                catch_up_specific_quorum: true,
                data_loss_replay: DataLossReplay::ReplaceOnAmbiguity,
                epoch,
                attempt_revision: 2,
            },
        };
        let members = vec![ConfigurationMember {
            identity: node.identity.clone(),
            role: ReplicaRole::Primary,
        }];
        let lifecycle = PublicLifecycleInput {
            recipe: PublicLifecycleRecipe::InitialPrimary,
            replica: node.identity.clone(),
            epoch,
            possible_data_loss: PossibleDataLossIntent::Possible,
            current: ConfigurationDescriptor::new(
                epoch,
                node.identity.replica_id,
                members.clone(),
                1,
            ),
            previous: Some(ConfigurationDescriptor::new(
                Epoch::new(1, 0),
                node.identity.replica_id,
                members,
                1,
            )),
            replicas: Vec::new(),
            data_loss_recovery: Some(PublicDataLossRecoveryInput {
                authorization: authorization.clone(),
                attempt: attempt.clone(),
            }),
        };
        let intent = PublicOperationIntent {
            identity: node.preview.clone(),
            operation_id: attempt.attempt_id.clone(),
            revision: 2,
            process_session_id: attempt.process.process_session_id.clone(),
            class: PublicOperationClass::Authority,
            input_digest: lifecycle.digest(),
            replay_disposition: PublicOperationReplayDisposition::DataLoss(
                DataLossReplay::ReplaceOnAmbiguity,
            ),
            lifecycle: Some(lifecycle),
            program: None,
        };
        let accepted = PublicV1AcceptedStatus {
            identity: node.preview.clone(),
            history_initialization_id: derive_history_initialization_id(
                &resource,
                PublicProtocolGeneration::new(1),
            ),
            history: node.history.clone(),
            data_loss_recovery: None,
            completed_operations: Vec::new(),
            extensions: Default::default(),
        };
        let transition = PublicV1AcceptedTransitionStatus {
            resource_version: "status-1".into(),
            evidence: accepted.clone(),
            intent: PublicV1AcceptedTransitionIntent {
                consumer: PublicV1EvidenceConsumer::DataLoss,
                operation_id: intent.operation_id.clone(),
                revision: intent.revision,
                call_kind: PublicV1CallKind::InitialPrimary,
                history: node.history.clone(),
                source: PublicV1ResourceAuthority {
                    storage_incarnation: node.storage.clone(),
                    resource_version: "storage-1".into(),
                },
                target: None,
                process_session_id: intent.process_session_id.clone(),
                input_digest: intent.input_digest.clone(),
                replay_disposition: intent.replay_disposition,
                previous_configuration_id: None,
                current_configuration_id: None,
                catch_up_mode: None,
                catch_up_specific_quorum: None,
            },
        };
        let evidence = Arc::new(
            PublicV1EvidenceState::new(
                transition,
                vec![PublicV1EvidenceResource {
                    storage_incarnation: node.storage.clone(),
                    resource_version: "storage-1".into(),
                    proof: Some(PublicV1ResourceProof {
                        history: node.history.clone(),
                        storage_incarnation: node.storage.clone(),
                    }),
                    disposition: PublicV1ResourceDisposition::Active,
                }],
            )
            .unwrap(),
        );
        let adapter = HostedPublicV1Harness::new(
            vec![HostedPublicV1Replica {
                identity: node.identity.clone(),
                replication_address: before.replication_address.clone().unwrap(),
                handle: node.handle(),
            }],
            Some(accepted.clone()),
            None,
            evidence,
        )
        .unwrap();
        let scope = PublicV1Scope {
            namespace: "testing".into(),
            name: "recovery".into(),
            resource_uid: resource,
        };
        let mut crossed = authorization.clone();
        crossed.authorization_id = OperationId::new("crossed-authorization");
        assert!(
            plan_public_v1_data_loss_recovery(
                &accepted, &before, &crossed, &attempt, &intent, false,
            )
            .is_err()
        );
        assert_eq!(
            node.service.provider.recovery_calls.load(Ordering::SeqCst),
            0
        );
        execute_public_v1_history(
            &adapter,
            &scope,
            plan_public_v1_data_loss_recovery(
                &accepted,
                &before,
                &authorization,
                &attempt,
                &intent,
                false,
            )
            .unwrap(),
        )
        .await
        .unwrap();
        let invalidated = adapter
            .observe_public_v1_status(&scope)
            .await
            .unwrap()
            .unwrap();
        node.service
            .provider
            .recovery_outcome
            .store(outcome, Ordering::Release);
        node.service.provider.recovery.hold();
        let dispatch = execute_public_v1_history(
            &adapter,
            &scope,
            plan_public_v1_data_loss_recovery(
                &invalidated,
                &before,
                &authorization,
                &attempt,
                &intent,
                false,
            )
            .unwrap(),
        );
        tokio::pin!(dispatch);
        tokio::select! {
            _ = node.service.provider.recovery.entered.notified() => {}
            result = &mut dispatch => panic!("recovery did not reach its provider callback: {result:?}"),
        }
        let pending = node.report().await;
        assert_eq!(pending.history, node.history);
        assert_ne!(pending.read_status, AccessStatus::Granted);
        assert_ne!(pending.write_status, AccessStatus::Granted);
        assert!(matches!(
            pending.progress.current_progress,
            ProgressQueryResult::NotQueried(_)
        ));
        assert!(matches!(
            pending.progress.catch_up_capability,
            ProgressQueryResult::NotQueried(_)
        ));
        node.service.provider.recovery.release();
        let result = dispatch.await;
        let report = node.report().await;
        let store = SqliteStore::open_public_v1(
            SqliteStore::metadata_database_path(&node.root),
            &node.preview,
        )
        .unwrap();
        let persisted = store.public_v1_history_snapshot().await.unwrap().recovery;
        assert_eq!(persisted, report.data_loss_recovery);
        drop(store);
        let reopened = SqliteStore::open_public_v1(
            SqliteStore::metadata_database_path(&node.root),
            &node.preview,
        )
        .unwrap();
        assert_eq!(
            reopened
                .public_v1_history_snapshot()
                .await
                .unwrap()
                .recovery,
            persisted
        );
        let wire = wire_report(&node).await;
        assert_eq!(wire.data_loss_recovery, persisted);
        assert_eq!(wire.progress, report.progress);
        assert_eq!(
            node.service.provider.recovery_calls.load(Ordering::SeqCst),
            1
        );
        if outcome < 2 {
            result.unwrap();
            let observation = persisted
                .as_ref()
                .unwrap()
                .observed_progress
                .as_ref()
                .unwrap();
            assert!(matches!(
                observation.current_progress,
                ProgressQueryResult::Value(_)
            ));
            assert_eq!(
                observation.catch_up_capability,
                ProgressQueryResult::NotReady
            );
            execute_public_v1_history(
                &adapter,
                &scope,
                plan_public_v1_recovery_report(&invalidated, &wire).unwrap(),
            )
            .await
            .unwrap();
            assert_eq!(
                adapter
                    .observe_public_v1_status(&scope)
                    .await
                    .unwrap()
                    .unwrap()
                    .data_loss_recovery
                    .as_ref()
                    .unwrap()
                    .observed_progress,
                Some(observation.clone())
            );
            assert_eq!(report.history, proposed);
            assert_eq!(
                report
                    .configuration
                    .as_ref()
                    .expect("admitted native configuration")
                    .voting
                    .current
                    .epoch
                    .data_loss_number,
                proposed.data_loss_number,
            );
            assert_eq!(
                report.data_loss_recovery.unwrap().stage,
                PublicDataLossRecoveryStage::Admitted
            );
            assert_eq!(report.write_status, AccessStatus::Granted);
        } else {
            assert!(result.is_err());
            assert_eq!(report.history, node.history);
            assert_ne!(report.write_status, AccessStatus::Granted);
            assert_eq!(
                report.data_loss_recovery.unwrap().stage,
                if outcome == 2 {
                    PublicDataLossRecoveryStage::Failed
                } else {
                    PublicDataLossRecoveryStage::ReplacementRequired
                },
                "typed provider recovery result was lost: {result:?}",
            );
        }
        node.stop().await;
    }
}
