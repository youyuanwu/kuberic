use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as ProcessMutex, Weak};

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::{Mutex, mpsc};

use crate::config::PgConfig;
use crate::generation::GenerationClock;
use crate::owned_command::{CapturedCommands, OwnedCommands};
use crate::owned_process::OwnedProcess;
use crate::owned_task::{PgTask, PgTasks};
use kuberic_runtime::replicator::StatefulServicePartition;

use tokio_util::sync::CancellationToken;

fn increment_generation(generation: &AtomicU64) -> Option<u64> {
    let mut current = generation.load(Ordering::Acquire);
    loop {
        let next = current.checked_add(1)?;
        match generation.compare_exchange_weak(current, next, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => return Some(next),
            Err(observed) => current = observed,
        }
    }
}

async fn helper_lease_invalid(
    valid: &crate::adapter::public_transport::GatewayValidity,
    cancellation: &CancellationToken,
) {
    let mut tick = tokio::time::interval(std::time::Duration::from_millis(20));
    loop {
        tick.tick().await;
        if !crate::adapter::public_transport::rewind_candidate_lease_valid(valid, cancellation)
            .await
        {
            return;
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PgProcessFault {
    Transient,
    Permanent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PgProcessState {
    Stopped,
    Initialized,
    Starting,
    Running,
    Stopping,
    Faulted,
}

#[derive(Clone)]
struct StartParameters {
    fault_tx: mpsc::Sender<kuberic_runtime::protocol::types::FaultType>,
    cancellation: Option<(Weak<PgInstanceManager>, CancellationToken)>,
    native_password: Option<String>,
}

pub(crate) struct GenerationFault {
    generation: u64,
    fault: kuberic_runtime::protocol::types::FaultType,
}

#[cfg(all(test, feature = "testing"))]
#[derive(Default)]
pub(crate) struct DriverExitGate {
    pub(crate) entered: tokio::sync::Notify,
    pub(crate) release: tokio::sync::Notify,
}

#[derive(Clone)]
pub(crate) struct GenerationLease(Arc<ProcessGeneration>);

impl GenerationLease {
    pub(crate) fn id(&self) -> u64 {
        self.0.id
    }
}

struct ProcessGeneration {
    id: u64,
    child: ProcessMutex<Option<OwnedProcess>>,
    helpers: Arc<OwnedCommands>,
    cleanup_lock: ProcessMutex<()>,
    cleanup_result: ProcessMutex<Option<Result<(), PgError>>>,
    state: ProcessMutex<PgProcessState>,
    shutdown: CancellationToken,
    tasks: Arc<PgTasks>,
    tasks_result: ProcessMutex<Option<Result<(), PgError>>>,
}

impl ProcessGeneration {
    fn new(id: u64, tasks: Arc<PgTasks>) -> Self {
        Self {
            id,
            child: ProcessMutex::new(None),
            helpers: Arc::new(OwnedCommands::default()),
            cleanup_lock: ProcessMutex::new(()),
            cleanup_result: ProcessMutex::new(None),
            state: ProcessMutex::new(PgProcessState::Stopped),
            shutdown: CancellationToken::new(),
            tasks,
            tasks_result: ProcessMutex::new(None),
        }
    }

    fn running(&self) -> bool {
        self.child
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|child| !child.exited().unwrap_or(false))
    }

    fn spawn_monitor(&self, future: impl Future<Output = ()> + Send + 'static) {
        let _cleanup = self.cleanup_lock.lock().unwrap();
        if self.shutdown.is_cancelled() || self.cleanup_result.lock().unwrap().is_some() {
            return;
        }
        self.tasks.retain(self.id, true, tokio::spawn(future));
    }

    fn retire(&self, commands: &CapturedCommands, data_dir: &Path) -> Result<(), PgError> {
        let _cleanup = self.cleanup_lock.lock().unwrap();
        tracing::debug!(
            generation = self.id,
            "retiring captured PostgreSQL generation"
        );
        let retained = self.cleanup_result.lock().unwrap().clone();
        if matches!(retained, Some(Ok(()))) {
            return Ok(());
        }
        self.shutdown.cancel();
        self.tasks.fence(self.id);
        let helpers = commands.terminate();
        let postgres = self
            .child
            .lock()
            .unwrap()
            .as_mut()
            .map_or(Ok(()), |child| child.stop(data_dir));
        if postgres.is_ok() {
            self.child.lock().unwrap().take();
        }
        let result = match helpers {
            Ok(()) => postgres,
            Err(error) => Err(error.with_cleanup(postgres)),
        };
        let result = match retained {
            Some(Err(error)) => Err(error),
            _ => result,
        };
        *self.state.lock().unwrap() = if result.is_ok() {
            PgProcessState::Stopped
        } else {
            PgProcessState::Faulted
        };
        *self.cleanup_result.lock().unwrap() = Some(result.clone());
        result
    }
}

struct CleanupTicket {
    phase: AtomicU8,
    generation: Arc<ProcessGeneration>,
    commands: CapturedCommands,
    previous: PgProcessState,
    launches_closed: Arc<AtomicBool>,
}

struct CleanupWaiter(Arc<CleanupTicket>);

struct Lifecycle<'a> {
    instance: &'a PgInstanceManager,
    _guard: tokio::sync::MutexGuard<'a, ()>,
}

impl Drop for Lifecycle<'_> {
    fn drop(&mut self) {
        if let Err(error) = publish_stopped_generation(
            &self.instance.current,
            &self.instance.next_generation,
            &self.instance.launches_closed,
            None,
        ) {
            tracing::error!(%error, "process generation publication failed closed");
        }
    }
}

fn publish_stopped_generation(
    current: &ProcessMutex<Arc<ProcessGeneration>>,
    next: &GenerationClock,
    closed: &AtomicBool,
    expected: Option<&Arc<ProcessGeneration>>,
) -> Result<(), PgError> {
    let mut current = current.lock().unwrap();
    if expected.is_some_and(|expected| !Arc::ptr_eq(&current, expected)) {
        return Ok(());
    }
    let retired = matches!(*current.cleanup_result.lock().unwrap(), Some(Ok(())))
        && matches!(*current.tasks_result.lock().unwrap(), Some(Ok(())))
        && *current.state.lock().unwrap() == PgProcessState::Stopped;
    if retired && !closed.load(Ordering::Acquire) && !next.is_retired() {
        match next.allocate() {
            Ok(id) => *current = Arc::new(ProcessGeneration::new(id, current.tasks.clone())),
            Err(error) => {
                closed.store(true, Ordering::Release);
                *current.state.lock().unwrap() = PgProcessState::Faulted;
                *current.cleanup_result.lock().unwrap() = Some(Err(error.clone()));
                return Err(error.with_generation(current.id));
            }
        }
    }
    Ok(())
}

impl Drop for CleanupWaiter {
    fn drop(&mut self) {
        if self
            .0
            .phase
            .compare_exchange(0, 2, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            self.0.commands.cancel_unstarted();
            if !self.0.launches_closed.load(Ordering::Acquire)
                && self.0.generation.cleanup_result.lock().unwrap().is_none()
            {
                *self.0.generation.state.lock().unwrap() = self.0.previous;
            }
        }
    }
}

/// Manages a PostgreSQL instance as a child process.
///
/// Wraps pg_ctl, initdb, pg_basebackup, pg_rewind, and the postgres
/// server process. Each instance keeps its Unix sockets in PGDATA/pg_stat_tmp,
/// a transient directory excluded by PostgreSQL backup and rewind.
pub struct PgInstanceManager {
    data_dir: PathBuf,
    pg_bin: PathBuf,
    port: u16,
    current: Arc<ProcessMutex<Arc<ProcessGeneration>>>,
    next_generation: Arc<GenerationClock>,
    helper_store: ProcessMutex<Option<Weak<crate::durable::PgDurableStore>>>,
    launches_closed: Arc<AtomicBool>,
    abort_generation: AtomicU64,
    config: PgConfig,
    lifecycle_lock: Arc<Mutex<()>>,
    fault_sink: ProcessMutex<Option<mpsc::Sender<GenerationFault>>>,
    start_parameters: Mutex<Option<StartParameters>>,
    pub(crate) access_lock: Mutex<()>,
    pub(crate) access_state: AtomicU8,
    access_generation: AtomicU64,
    #[cfg(feature = "testing")]
    cleanup_hook: ProcessMutex<Option<CleanupHook>>,
    #[cfg(feature = "testing")]
    error_hook: ProcessMutex<Option<Arc<ErrorGate>>>,
    #[cfg(feature = "testing")]
    fault_hook: ProcessMutex<Option<Arc<ErrorGate>>>,
    #[cfg(feature = "testing")]
    exit_observer_hook: ProcessMutex<Option<Arc<ErrorGate>>>,
    #[cfg(test)]
    pub(crate) clear_pgdata_hook: ProcessMutex<Option<(String, PathBuf)>>,
}

#[cfg(feature = "testing")]
pub struct CleanupGate {
    pub entered: Arc<tokio::sync::Notify>,
    pub finished: Arc<tokio::sync::Notify>,
    release: std::sync::mpsc::Sender<()>,
}

#[cfg(feature = "testing")]
pub struct ErrorGate {
    pub entered: tokio::sync::Notify,
    pub release: tokio::sync::Notify,
}

#[cfg(feature = "testing")]
impl CleanupGate {
    pub fn release(&self) {
        let _ = self.release.send(());
    }
}

#[cfg(feature = "testing")]
struct CleanupHook {
    entered: Arc<tokio::sync::Notify>,
    finished: Arc<tokio::sync::Notify>,
    release: std::sync::mpsc::Receiver<()>,
}

impl PgInstanceManager {
    #[cfg(feature = "testing")]
    pub fn pause_error_handling(&self) -> Arc<ErrorGate> {
        let gate = Arc::new(ErrorGate {
            entered: tokio::sync::Notify::new(),
            release: tokio::sync::Notify::new(),
        });
        *self.error_hook.lock().unwrap() = Some(gate.clone());
        gate
    }

    #[cfg(feature = "testing")]
    pub fn pause_fault_delivery(&self) -> Arc<ErrorGate> {
        let gate = Arc::new(ErrorGate {
            entered: tokio::sync::Notify::new(),
            release: tokio::sync::Notify::new(),
        });
        *self.fault_hook.lock().unwrap() = Some(gate.clone());
        gate
    }

    #[cfg(feature = "testing")]
    pub fn pause_exit_observer_registration(&self) -> Arc<ErrorGate> {
        let gate = Arc::new(ErrorGate {
            entered: tokio::sync::Notify::new(),
            release: tokio::sync::Notify::new(),
        });
        *self.exit_observer_hook.lock().unwrap() = Some(gate.clone());
        gate
    }

    #[cfg(feature = "testing")]
    pub fn pause_cleanup(&self) -> CleanupGate {
        let entered = Arc::new(tokio::sync::Notify::new());
        let finished = Arc::new(tokio::sync::Notify::new());
        let (release, receiver) = std::sync::mpsc::channel();
        *self.cleanup_hook.lock().unwrap() = Some(CleanupHook {
            entered: entered.clone(),
            finished: finished.clone(),
            release: receiver,
        });
        CleanupGate {
            entered,
            finished,
            release,
        }
    }

    pub fn new(data_dir: PathBuf, pg_bin: PathBuf, port: u16) -> Self {
        let config = PgConfig::new(port, &data_dir);
        Self {
            data_dir,
            pg_bin,
            port,
            current: Arc::new(ProcessMutex::new(Arc::new(ProcessGeneration::new(
                0,
                Arc::new(PgTasks::default()),
            )))),
            next_generation: Arc::new(GenerationClock::default()),
            launches_closed: Arc::new(AtomicBool::new(false)),
            abort_generation: AtomicU64::new(0),
            config,
            helper_store: ProcessMutex::new(None),
            lifecycle_lock: Arc::new(Mutex::new(())),
            fault_sink: ProcessMutex::new(None),
            start_parameters: Mutex::new(None),
            access_lock: Mutex::new(()),
            access_state: AtomicU8::new(crate::access::CLOSED),
            access_generation: AtomicU64::new(0),
            #[cfg(feature = "testing")]
            cleanup_hook: ProcessMutex::new(None),
            #[cfg(feature = "testing")]
            error_hook: ProcessMutex::new(None),
            #[cfg(feature = "testing")]
            fault_hook: ProcessMutex::new(None),
            #[cfg(feature = "testing")]
            exit_observer_hook: ProcessMutex::new(None),
            #[cfg(test)]
            clear_pgdata_hook: ProcessMutex::new(None),
        }
    }

    fn generation(&self) -> Arc<ProcessGeneration> {
        self.current.lock().unwrap().clone()
    }

    async fn lifecycle(&self) -> Lifecycle<'_> {
        Lifecycle {
            instance: self,
            _guard: self.lifecycle_lock.lock().await,
        }
    }

    pub(crate) fn bind_fault_sink(&self, sink: mpsc::Sender<GenerationFault>) {
        *self.fault_sink.lock().unwrap() = Some(sink);
    }

    pub(crate) async fn deliver_fault(
        &self,
        notice: GenerationFault,
        partition: &StatefulServicePartition,
    ) {
        #[cfg(feature = "testing")]
        {
            let gate = self.fault_hook.lock().unwrap().take();
            if let Some(gate) = gate {
                gate.entered.notify_one();
                gate.release.notified().await;
            }
        }
        let _lifecycle = self.lifecycle().await;
        let generation = self.generation();
        if generation.id == notice.generation
            && !matches!(
                *generation.state.lock().unwrap(),
                PgProcessState::Stopping | PgProcessState::Stopped
            )
        {
            let _ = partition.report_fault(notice.fault).await;
        }
    }

    pub(crate) fn generation_id(&self) -> u64 {
        self.generation().id
    }

    pub(crate) fn access_generation(&self) -> u64 {
        self.access_generation.load(Ordering::Acquire)
    }

    pub(crate) fn advance_access_generation(&self) -> Result<u64, PgError> {
        increment_generation(&self.access_generation)
            .ok_or_else(|| PgError::GenerationExhausted("access generation".into()))
    }

    pub(crate) async fn bind_generation_store(
        &self,
        store: &Arc<crate::durable::PgDurableStore>,
    ) -> Result<(), PgError> {
        let _lifecycle = self.lifecycle().await;
        *self.helper_store.lock().unwrap() = Some(Arc::downgrade(store));
        store.ensure_helpers_complete().await?;
        self.next_generation.bind(store).await?;
        let id = self.next_generation.allocate()?;
        let mut current = self.current.lock().unwrap();
        *current = Arc::new(ProcessGeneration::new(id, current.tasks.clone()));
        Ok(())
    }

    fn native_helper_store(&self) -> Result<Option<Arc<crate::durable::PgDurableStore>>, PgError> {
        self.helper_store
            .lock()
            .unwrap()
            .as_ref()
            .map(|store| {
                store
                    .upgrade()
                    .ok_or_else(|| PgError::Process("native helper store retired".into()))
            })
            .transpose()
    }

    pub(crate) async fn ensure_native_helpers_complete(&self) -> Result<(), PgError> {
        if let Some(store) = self.native_helper_store()? {
            let generation = self.generation();
            let current = {
                let child = generation.child.lock().unwrap();
                match child.as_ref() {
                    Some(child) if !child.exited()? => child.postmaster_selection().cloned(),
                    _ => None,
                }
            };
            if current.as_ref().is_some_and(|selection| {
                selection.launch.intent.process_generation != generation.id
            }) {
                return Err(PgError::Process(
                    "current postmaster generation changed".into(),
                ));
            }
            store.ensure_executions_complete(current.as_ref()).await?;
        }
        Ok(())
    }

    #[cfg(feature = "testing")]
    pub fn advance_generation_for_test(&self, value: u64) -> Result<(), PgError> {
        self.next_generation.advance_for_test(value)
    }

    #[cfg(feature = "testing")]
    pub fn process_generation_for_test(&self) -> u64 {
        self.generation_id()
    }

    pub(crate) fn generation_lease(&self) -> GenerationLease {
        GenerationLease(self.generation())
    }

    pub(crate) fn generation_is_current(&self, lease: &GenerationLease) -> bool {
        Arc::ptr_eq(&self.generation(), &lease.0)
            && lease.0.cleanup_result.lock().unwrap().is_none()
            && *lease.0.state.lock().unwrap() != PgProcessState::Stopping
    }

    pub(crate) async fn generation_step<T>(
        &self,
        lease: &GenerationLease,
        operation: impl Future<Output = Result<T, PgError>>,
    ) -> Result<T, PgError> {
        let _lifecycle = self.lifecycle().await;
        if !self.generation_is_current(lease) {
            return Err(
                PgError::Process("operation generation retired before mutation".into())
                    .with_generation(lease.id()),
            );
        }
        self.complete_generation(&lease.0, operation.await)
    }

    pub(crate) async fn generation_operation<T>(
        &self,
        operation: impl Future<Output = Result<T, PgError>>,
    ) -> Result<T, PgError> {
        let generation = self.generation();
        self.complete_generation(&generation, operation.await)
    }

    fn complete_generation<T>(
        &self,
        generation: &Arc<ProcessGeneration>,
        result: Result<T, PgError>,
    ) -> Result<T, PgError> {
        let result = result.and_then(|value| {
            if !Arc::ptr_eq(&self.generation(), generation)
                || generation.cleanup_result.lock().unwrap().is_some()
            {
                Err(PgError::Process(
                    "operation completed after its generation retired".into(),
                ))
            } else {
                Ok(value)
            }
        });
        result.map_err(|error| error.with_generation(generation.id))
    }

    async fn command_output(
        &self,
        generation: &Arc<ProcessGeneration>,
        command: &mut Command,
        deadline: Option<(std::time::Duration, &'static str)>,
    ) -> Result<std::process::Output, PgError> {
        self.ensure_native_helpers_complete().await?;
        let result = match deadline {
            Some((duration, name)) => {
                generation
                    .helpers
                    .output_with_timeout(command, duration, name)
                    .await
            }
            None => generation.helpers.output(command).await,
        };
        self.complete_generation(generation, result)
    }

    pub(crate) async fn handle_error<F, Fut>(
        &self,
        error: PgError,
        access_generation: Option<u64>,
        report: F,
    ) -> kuberic_runtime::RuntimeError
    where
        F: Fn(kuberic_runtime::protocol::types::FaultType, PgError) -> Fut,
        Fut: Future<Output = kuberic_runtime::RuntimeError>,
    {
        #[cfg(feature = "testing")]
        {
            let gate = self.error_hook.lock().unwrap().take();
            if let Some(gate) = gate {
                gate.entered.notify_one();
                gate.release.notified().await;
            }
        }
        if access_generation.is_some_and(|generation| generation != self.access_generation()) {
            return kuberic_runtime::RuntimeError::OperationCancelled;
        }
        let _lifecycle = self.lifecycle().await;
        let generation = self.generation();
        let state = *generation.state.lock().unwrap();
        let retired = state == PgProcessState::Stopping
            || (state == PgProcessState::Stopped && generation.shutdown.is_cancelled());
        if error.origin().is_some_and(|origin| origin != generation.id)
            || (error.origin().is_some() && retired)
        {
            tracing::debug!(%error, current_generation = generation.id, "ignoring retired operation fault");
            return kuberic_runtime::RuntimeError::OperationCancelled;
        }
        let transient = error.is_timeout();
        let cleanup = if transient {
            self.retire_generation(generation.clone(), false).await
        } else {
            let Some(result) = self.abort_generation(&generation) else {
                tracing::debug!(%error, "ignoring replaced operation fault");
                return kuberic_runtime::RuntimeError::OperationCancelled;
            };
            result
        };
        if !Arc::ptr_eq(&self.generation(), &generation) {
            tracing::debug!(%error, "ignoring replaced operation fault");
            return kuberic_runtime::RuntimeError::OperationCancelled;
        }
        let error = error.with_cleanup(cleanup);
        let recovered = transient && error.is_timeout();
        let result = report(error.fault_type(), error).await;
        if recovered {
            if let Err(error) = publish_stopped_generation(
                &self.current,
                &self.next_generation,
                &self.launches_closed,
                Some(&generation),
            ) {
                return report(error.fault_type(), error).await;
            }
            tracing::warn!(%result, generation = generation.id, "failed operation retired; successor cleanup is not authorized");
            return kuberic_runtime::RuntimeError::OperationCancelled;
        }
        result
    }

    fn replace_retired(&self, expected_abort: u64) -> Result<Arc<ProcessGeneration>, PgError> {
        let mut current = self.current.lock().unwrap();
        if expected_abort == u64::MAX {
            return Err(PgError::GenerationExhausted("process abort epoch".into()));
        }
        if self.abort_generation.load(Ordering::Acquire) != expected_abort {
            return Err(PgError::Process(
                "owned PostgreSQL startup was aborted".into(),
            ));
        }
        if current.cleanup_result.lock().unwrap().is_some()
            && !matches!(*current.tasks_result.lock().unwrap(), Some(Ok(())))
        {
            return Err(PgError::Process(
                "retired Rust descendants are not joined".into(),
            ));
        }
        let id = self.next_generation.allocate().map_err(|error| {
            self.launches_closed.store(true, Ordering::Release);
            *current.state.lock().unwrap() = PgProcessState::Faulted;
            *current.cleanup_result.lock().unwrap() = Some(Err(error.clone()));
            error.with_generation(current.id)
        })?;
        let generation = Arc::new(ProcessGeneration::new(id, current.tasks.clone()));
        *current = generation.clone();
        self.launches_closed.store(false, Ordering::Release);
        Ok(generation)
    }

    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// Host address PG listens on for TCP connections.
    /// For local tests: 127.0.0.1. For K8s: pod IP (configurable).
    pub fn listen_host(&self) -> &str {
        "127.0.0.1"
    }

    /// Transient socket directory within the owned data directory.
    pub fn socket_dir(&self) -> &Path {
        Path::new(&self.config.socket_dir)
    }

    /// Connection string for local UDS access.
    /// Uses the current OS user (initdb creates a superuser matching the OS user).
    pub fn connection_string(&self) -> String {
        format!(
            "host={} port={} dbname=postgres",
            self.socket_dir().display(),
            self.port,
        )
    }

    pub fn application_connection_string(&self) -> String {
        format!(
            "host={} port={} dbname={} user={}",
            self.socket_dir().display(),
            self.port,
            crate::access::APPLICATION_DATABASE,
            crate::access::APPLICATION_ROLE,
        )
    }

    /// Initialize a new PG cluster with data checksums.
    pub async fn init_db(&self) -> Result<(), PgError> {
        let _lifecycle = self.lifecycle().await;
        let generation = self.generation();
        let output = self
            .command_output(
                &generation,
                Command::new(self.pg_bin.join("initdb"))
                    .args([
                        "--data-checksums",
                        "-D",
                        &self.data_dir.to_string_lossy(),
                        "--auth=trust",
                        "--no-instructions",
                    ])
                    .env("LC_ALL", "C"),
                None,
            )
            .await?;

        if !output.status.success() {
            return Err(
                PgError::command_failed("initdb failed", &output).with_generation(generation.id)
            );
        }

        // Write required config
        self.config
            .write_initial(&self.data_dir)
            .await
            .map_err(|error| error.with_generation(generation.id))?;
        *generation.state.lock().unwrap() = PgProcessState::Initialized;

        tracing::info!(
            data_dir = %self.data_dir.display(),
            port = self.port,
            "initdb complete"
        );
        Ok(())
    }

    /// Access the PgConfig for post-clone patching.
    pub fn config(&self) -> &PgConfig {
        &self.config
    }

    pub async fn start_native(
        &self,
        fault_tx: mpsc::Sender<kuberic_runtime::protocol::types::FaultType>,
    ) -> Result<(), PgError> {
        self.start_with_parameters(StartParameters {
            fault_tx,
            cancellation: None,
            native_password: None,
        })
        .await
    }

    fn fault_reporter(
        &self,
        fault_tx: mpsc::Sender<kuberic_runtime::protocol::types::FaultType>,
        generation: &Arc<ProcessGeneration>,
    ) -> Arc<dyn Fn(PgProcessFault) + Send + Sync> {
        let current = Arc::downgrade(&self.current);
        let sink = self.fault_sink.lock().unwrap().clone();
        let generation = Arc::downgrade(generation);
        Arc::new(move |fault| {
            let fault = match fault {
                PgProcessFault::Transient => kuberic_runtime::protocol::types::FaultType::Transient,
                PgProcessFault::Permanent => kuberic_runtime::protocol::types::FaultType::Permanent,
            };
            if let (Some(current), Some(generation)) = (current.upgrade(), generation.upgrade())
                && Arc::ptr_eq(&current.lock().unwrap(), &generation)
            {
                if let Some(sink) = &sink {
                    let notice = GenerationFault {
                        generation: generation.id,
                        fault,
                    };
                    match sink.try_send(notice) {
                        Ok(()) | Err(mpsc::error::TrySendError::Closed(_)) => {}
                        Err(mpsc::error::TrySendError::Full(notice)) => {
                            let sink = sink.clone();
                            tokio::spawn(async move {
                                let _ = sink.send(notice).await;
                            });
                        }
                    }
                    return;
                }
                match fault_tx.try_send(fault) {
                    Ok(()) | Err(mpsc::error::TrySendError::Closed(_)) => {}
                    Err(mpsc::error::TrySendError::Full(fault)) => {
                        let fault_tx = fault_tx.clone();
                        tokio::spawn(async move {
                            if let Ok(permit) = fault_tx.reserve().await
                                && Arc::ptr_eq(&current.lock().unwrap(), &generation)
                            {
                                permit.send(fault);
                            }
                        });
                    }
                }
            }
        })
    }

    pub async fn start_native_with_cancellation(
        self: &Arc<Self>,
        fault_tx: mpsc::Sender<kuberic_runtime::protocol::types::FaultType>,
        cancellation: tokio_util::sync::CancellationToken,
    ) -> Result<(), PgError> {
        self.start_with_parameters(StartParameters {
            fault_tx,
            cancellation: Some((Arc::downgrade(self), cancellation)),
            native_password: None,
        })
        .await
    }

    pub(crate) async fn start_public_receiver(
        self: &Arc<Self>,
        fault_tx: mpsc::Sender<kuberic_runtime::protocol::types::FaultType>,
        cancellation: CancellationToken,
        credential: &crate::adapter::public_transport::NativeCredential,
    ) -> Result<(), PgError> {
        credential
            .helper_conninfo()
            .map_err(|error| PgError::Configuration(error.to_string()))?;
        self.start_with_parameters(StartParameters {
            fault_tx,
            cancellation: Some((Arc::downgrade(self), cancellation)),
            native_password: Some(credential.secret.clone()),
        })
        .await
    }

    pub(crate) async fn restart_access_closed(&self) -> Result<(), PgError> {
        let parameters = self
            .start_parameters
            .lock()
            .await
            .clone()
            .ok_or_else(|| PgError::Process("no owned PostgreSQL run to fence".into()))?;
        self.stop().await?;
        self.start_with_parameters(parameters).await
    }

    pub async fn control_identity(&self) -> Result<(String, u32), PgError> {
        let generation = self.generation();
        let result = async {
            let output = self
                .command_output(
                    &generation,
                    Command::new(self.pg_bin.join("pg_controldata"))
                        .arg(&self.data_dir)
                        .env("LC_ALL", "C"),
                    Some((std::time::Duration::from_secs(2), "pg_controldata")),
                )
                .await?;
            if !output.status.success() {
                return Err(PgError::command_failed(
                    "cannot inspect PostgreSQL control data",
                    &output,
                ));
            }
            let text = String::from_utf8_lossy(&output.stdout);
            let field = |name: &str| {
                text.lines()
                    .find_map(|line| line.strip_prefix(name))
                    .map(str::trim)
                    .ok_or_else(|| PgError::Process(format!("missing control field {name}")))
            };
            let system = field("Database system identifier:")?.to_owned();
            let timeline: u32 = field("Latest checkpoint's TimeLineID:")?
                .parse()
                .map_err(|_| PgError::Process("invalid control timeline".into()))?;
            let recovery_timeline: u32 = field("Min recovery ending loc's timeline:")?
                .parse()
                .map_err(|_| PgError::Process("invalid recovery control timeline".into()))?;
            let timeline = timeline.max(recovery_timeline);
            Ok((system, timeline))
        }
        .await;
        self.complete_generation(&generation, result)
    }

    pub(crate) async fn clean_primary_shutdown_identity(&self) -> Result<(String, u32), PgError> {
        let _lifecycle = self.lifecycle().await;
        let generation = self.generation();
        if generation.running() || self.data_dir.join("postmaster.pid").exists() {
            return Err(PgError::Process(
                "quiescent PostgreSQL still has a postmaster owner".into(),
            ));
        }
        let output = self
            .command_output(
                &generation,
                Command::new(self.pg_bin.join("pg_controldata"))
                    .arg(&self.data_dir)
                    .env("LC_ALL", "C"),
                Some((
                    std::time::Duration::from_secs(2),
                    "quiescent pg_controldata",
                )),
            )
            .await?;
        if !output.status.success() {
            return Err(PgError::command_failed(
                "cannot inspect quiescent PostgreSQL",
                &output,
            ));
        }
        let text = String::from_utf8_lossy(&output.stdout);
        let field = |name: &str| {
            text.lines()
                .find_map(|line| line.strip_prefix(name))
                .map(str::trim)
                .ok_or_else(|| PgError::Process(format!("missing control field {name}")))
        };
        if field("Database cluster state:")? != "shut down"
            || field("Data page checksum version:")? != "1"
            || field("Latest checkpoint's full_page_writes:")? != "on"
        {
            return Err(PgError::Process(
                "retained primary is not cleanly shut down with checksums/full-page writes".into(),
            ));
        }
        let system = field("Database system identifier:")?.to_owned();
        let timeline = field("Latest checkpoint's TimeLineID:")?
            .parse()
            .map_err(|_| PgError::Process("invalid quiescent control timeline".into()))?;
        self.complete_generation(&generation, Ok((system, timeline)))
    }

    pub(crate) async fn retained_wal_boundary(&self) -> Result<i64, PgError> {
        let generation = self.generation();
        let result = async {
            let output = self
                .command_output(
                    &generation,
                    Command::new(self.pg_bin.join("pg_controldata"))
                        .arg(&self.data_dir)
                        .env("LC_ALL", "C"),
                    Some((std::time::Duration::from_secs(2), "pg_controldata")),
                )
                .await?;
            if !output.status.success() {
                return Err(PgError::command_failed(
                    "cannot inspect WAL segment size",
                    &output,
                ));
            }
            let text = String::from_utf8_lossy(&output.stdout);
            let size: u64 = text
                .lines()
                .find_map(|line| line.strip_prefix("Bytes per WAL segment:"))
                .and_then(|value| value.trim().parse().ok())
                .filter(|size: &u64| {
                    size.is_power_of_two() && *size >= 1024 * 1024 && *size <= 1024 * 1024 * 1024
                })
                .ok_or_else(|| PgError::Process("invalid WAL segment size".into()))?;
            let mut entries = tokio::fs::read_dir(self.data_dir.join("pg_wal"))
                .await
                .map_err(|error| PgError::Process(format!("inspect retained WAL: {error}")))?;
            let mut oldest = None;
            while let Some(entry) = entries
                .next_entry()
                .await
                .map_err(|error| PgError::Process(format!("inspect WAL entry: {error}")))?
            {
                let name = entry.file_name();
                let Some(name) = name.to_str() else { continue };
                let name = name.strip_suffix(".partial").unwrap_or(name);
                if name.len() != 24 || !name.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                    continue;
                }
                let log = u64::from_str_radix(&name[8..16], 16)
                    .map_err(|error| PgError::Process(error.to_string()))?;
                let segment = u64::from_str_radix(&name[16..24], 16)
                    .map_err(|error| PgError::Process(error.to_string()))?;
                if segment >= (1_u64 << 32) / size {
                    return Err(PgError::Process("invalid retained WAL segment".into()));
                }
                let boundary = log
                    .checked_mul(1_u64 << 32)
                    .and_then(|log| {
                        segment
                            .checked_mul(size)
                            .and_then(|segment| log.checked_add(segment))
                    })
                    .and_then(|position| i64::try_from(position).ok())
                    .ok_or_else(|| PgError::Process("retained WAL position overflow".into()))?;
                oldest = Some(oldest.map_or(boundary, |old: i64| old.min(boundary)));
            }
            oldest.ok_or_else(|| PgError::Process("no retained PostgreSQL WAL".into()))
        }
        .await;
        self.complete_generation(&generation, result)
    }

    pub(crate) async fn retained_standby_identity(&self) -> Result<(String, u32), PgError> {
        let _lifecycle = self.lifecycle().await;
        let generation = self.generation();
        let result = async {
            if generation.running()
                || self
                    .data_dir
                    .join("postmaster.pid")
                    .try_exists()
                    .map_err(|error| {
                        PgError::Process(format!("inspect retained postmaster: {error}"))
                    })?
                || self
                    .data_dir
                    .join("recovery.signal")
                    .try_exists()
                    .map_err(|error| {
                        PgError::Process(format!("inspect retained recovery signal: {error}"))
                    })?
            {
                return Err(PgError::Process(
                    "retained standby still has a postmaster or non-standby recovery owner".into(),
                ));
            }
            let signal = tokio::fs::symlink_metadata(self.data_dir.join("standby.signal"))
                .await
                .map_err(|error| PgError::Process(format!("inspect retained standby: {error}")))?;
            if !signal.is_file() {
                return Err(PgError::Process(
                    "retained standby signal is not a file".into(),
                ));
            }
            let version = tokio::fs::read_to_string(self.data_dir.join("PG_VERSION"))
                .await
                .map_err(|error| {
                    PgError::Process(format!("read retained data version: {error}"))
                })?;
            let binary = self
                .command_output(
                    &generation,
                    Command::new(self.pg_bin.join("postgres"))
                        .arg("--version")
                        .env("LC_ALL", "C"),
                    Some((
                        std::time::Duration::from_secs(2),
                        "retained postgres version",
                    )),
                )
                .await?;
            let binary_version = String::from_utf8_lossy(&binary.stdout);
            let major = binary_version
                .strip_prefix("postgres (PostgreSQL) ")
                .and_then(|version| version.split('.').next());
            if !binary.status.success() || major != Some(version.trim()) {
                return Err(PgError::Process(
                    "retained standby data format differs from the native binary".into(),
                ));
            }
            let output = self
                .command_output(
                    &generation,
                    Command::new(self.pg_bin.join("pg_controldata"))
                        .arg(&self.data_dir)
                        .env("LC_ALL", "C"),
                    Some((
                        std::time::Duration::from_secs(2),
                        "retained standby pg_controldata",
                    )),
                )
                .await?;
            if !output.status.success() {
                return Err(PgError::command_failed(
                    "cannot inspect retained standby",
                    &output,
                ));
            }
            let text = String::from_utf8_lossy(&output.stdout);
            let field = |name: &str| {
                text.lines()
                    .find_map(|line| line.strip_prefix(name))
                    .map(str::trim)
                    .ok_or_else(|| PgError::Process(format!("missing control field {name}")))
            };
            if field("Database cluster state:")? != "shut down in recovery"
                || field("Data page checksum version:")? != "1"
                || field("Latest checkpoint's full_page_writes:")? != "on"
            {
                return Err(PgError::Process(
                    "retained native data is not a stopped standby with checksums/full-page writes"
                        .into(),
                ));
            }
            let system = field("Database system identifier:")?.to_owned();
            let checkpoint: u32 = field("Latest checkpoint's TimeLineID:")?
                .parse()
                .map_err(|_| PgError::Process("invalid retained standby timeline".into()))?;
            let recovery: u32 = field("Min recovery ending loc's timeline:")?
                .parse()
                .map_err(|_| PgError::Process("invalid retained recovery timeline".into()))?;
            Ok((system, checkpoint.max(recovery)))
        }
        .await;
        self.complete_generation(&generation, result)
    }

    async fn start_with_parameters(&self, parameters: StartParameters) -> Result<(), PgError> {
        let _lifecycle = self.lifecycle().await;
        let expected_abort = self.abort_generation.load(Ordering::Acquire);
        if parameters
            .cancellation
            .as_ref()
            .is_some_and(|(_, token)| token.is_cancelled())
        {
            return Err(PgError::Process("PostgreSQL run was cancelled".into()));
        }
        let existing = self.generation();
        if let Some(Err(error)) = existing.cleanup_result.lock().unwrap().clone() {
            return Err(error.with_generation(existing.id));
        }
        self.ensure_native_helpers_complete().await?;
        if existing.running() && *existing.state.lock().unwrap() == PgProcessState::Running {
            return Ok(());
        }
        *self.start_parameters.lock().await = Some(parameters.clone());
        self.finish_owned().await?;
        let generation = self.replace_retired(expected_abort)?;
        let store = self.native_helper_store()?;
        let _selection = match &store {
            Some(store) => Some(store.helper_lock.lock().await),
            None => None,
        };
        if let Some(store) = &store {
            store.ensure_helpers_complete().await?;
        }
        let fault_reporter = self.fault_reporter(parameters.fault_tx.clone(), &generation);
        *generation.state.lock().unwrap() = PgProcessState::Starting;
        let prepared = async {
            OwnedProcess::check_support()?;
            self.control_identity().await?;
            self.config.write_initial(&self.data_dir).await?;
            self.access_state
                .store(crate::access::CLOSED, Ordering::Release);
            Ok::<_, PgError>(())
        }
        .await;
        if let Err(error) = prepared {
            fault_reporter(match error.fault_type() {
                kuberic_runtime::protocol::types::FaultType::Transient => PgProcessFault::Transient,
                kuberic_runtime::protocol::types::FaultType::Permanent => PgProcessFault::Permanent,
            });
            *generation.state.lock().unwrap() = PgProcessState::Faulted;
            return Err(error.with_generation(generation.id));
        }

        let (stdout, stderr) = match self
            .spawn_owned(
                &generation,
                parameters.native_password.as_deref(),
                store.as_deref(),
            )
            .await
        {
            Ok(streams) => streams,
            Err(error) => {
                fault_reporter(PgProcessFault::Permanent);
                *generation.state.lock().unwrap() = PgProcessState::Faulted;
                let cleanup = self.retire_owned(false).await;
                return Err(error.with_cleanup(cleanup).with_generation(generation.id));
            }
        };
        drop(_selection);

        // Forward stdout + stderr through tracing in one task
        let stdout = BufReader::new(stdout);
        let stderr = BufReader::new(stderr);
        generation.spawn_monitor(async move {
            let mut stdout_lines = stdout.lines();
            let mut stderr_lines = stderr.lines();
            loop {
                tokio::select! {
                    result = stderr_lines.next_line() => {
                        match result {
                            Ok(Some(line)) => tracing::info!(target: "postgres", "{}", line),
                            _ => break,
                        }
                    }
                    result = stdout_lines.next_line() => {
                        match result {
                            Ok(Some(line)) => tracing::debug!(target: "postgres", "{}", line),
                            _ => break,
                        }
                    }
                }
            }
        });

        // Fresh shutdown token for this run
        let shutdown = generation.shutdown.clone();

        // Wait for PG to be ready
        if let Err(error) = self.wait_ready(&generation).await {
            fault_reporter(PgProcessFault::Permanent);
            let cleanup = self.retire_owned(false).await;
            *generation.state.lock().unwrap() = PgProcessState::Faulted;
            return Err(error.with_cleanup(cleanup).with_generation(generation.id));
        }

        #[cfg(feature = "testing")]
        let exit_observer_hook = self.exit_observer_hook.lock().unwrap().take();
        #[cfg(feature = "testing")]
        if let Some(hook) = exit_observer_hook {
            hook.entered.notify_one();
            hook.release.notified().await;
        }

        let exit_observer = {
            let _cleanup = generation.cleanup_lock.lock().unwrap();
            if self.launches_closed.load(Ordering::Acquire)
                || generation.cleanup_result.lock().unwrap().is_some()
            {
                Ok(None)
            } else {
                let child = generation.child.lock().unwrap();
                child
                    .as_ref()
                    .ok_or_else(|| PgError::Process("PostgreSQL ownership lost".into()))
                    .and_then(OwnedProcess::exit_observer)
                    .map(|observer| {
                        *generation.state.lock().unwrap() = PgProcessState::Running;
                        Some(observer)
                    })
            }
        };
        let exit_observer = match exit_observer {
            Ok(Some(observer)) => observer,
            Ok(None) => {
                return Err(PgError::Process("PostgreSQL run was cancelled".into())
                    .with_generation(generation.id));
            }
            Err(error) => {
                fault_reporter(PgProcessFault::Permanent);
                let cleanup = self.retire_owned(false).await;
                *generation.state.lock().unwrap() = PgProcessState::Faulted;
                return Err(error.with_cleanup(cleanup).with_generation(generation.id));
            }
        };

        // Observe duplicated pidfds without reaping or contending with cleanup.
        {
            let observed = generation.clone();
            let exit_shutdown = shutdown.clone();
            let exit_fault_reporter = fault_reporter.clone();
            let exit_port = self.port;
            generation.spawn_monitor(async move {
                let result = tokio::select! {
                    biased;
                    _ = exit_shutdown.cancelled() => return,
                    result = exit_observer.wait() => result,
                };
                if !exit_shutdown.is_cancelled() {
                    match result {
                        Ok(()) => {
                            tracing::error!(port = exit_port, "PostgreSQL exited unexpectedly")
                        }
                        Err(error) => tracing::error!(
                            port = exit_port,
                            %error,
                            "PostgreSQL exit observation failed"
                        ),
                    }
                    exit_fault_reporter(PgProcessFault::Permanent);
                    *observed.state.lock().unwrap() = PgProcessState::Faulted;
                }
            });
        }

        // Spawn pg_isready health monitor — catches PG hung but process alive
        // (complements the exit monitor above). Exits quietly on shutdown.
        let port = self.port;
        let health_fault_reporter = fault_reporter;
        let data_dir = self.socket_dir().to_path_buf();
        let pg_bin = self.pg_bin.clone();
        let shutdown = shutdown.clone();
        let health_generation = generation.clone();
        let helpers = generation.helpers.clone();
        generation.spawn_monitor(async move {
            let mut consecutive_failures: u32 = 0;
            loop {
                tokio::select! {
                    _ = shutdown.cancelled() => break,
                    _ = tokio::time::sleep(std::time::Duration::from_secs(2)) => {}
                }
                if *health_generation.state.lock().unwrap() != PgProcessState::Running {
                    continue;
                }
                let status = helpers
                    .output(Command::new(pg_bin.join("pg_isready")).args([
                        "-h",
                        &data_dir.to_string_lossy(),
                        "-p",
                        &port.to_string(),
                        "-d",
                        "postgres",
                    ]))
                    .await;
                match status {
                    Ok(output) if output.status.success() => {
                        consecutive_failures = 0;
                    }
                    _ => {
                        consecutive_failures += 1;
                        if consecutive_failures >= 3 {
                            if !shutdown.is_cancelled() {
                                tracing::error!(
                                    port,
                                    "PostgreSQL unresponsive (3 consecutive pg_isready failures)"
                                );
                                health_fault_reporter(PgProcessFault::Transient);
                                *health_generation.state.lock().unwrap() = PgProcessState::Faulted;
                            }
                            break;
                        }
                    }
                }
            }
        });

        if let Some((instance, cancellation)) = parameters.cancellation {
            let observed = generation.clone();
            generation.spawn_monitor(async move {
                cancellation.cancelled().await;
                if let Some(instance) = instance.upgrade() {
                    let _lifecycle = instance.lifecycle().await;
                    if Arc::ptr_eq(&instance.generation(), &observed) {
                        let result = instance.abort_owned();
                        *observed.state.lock().unwrap() = if result.is_ok() {
                            PgProcessState::Stopped
                        } else {
                            PgProcessState::Faulted
                        };
                    }
                }
            });
        }
        tracing::info!(port = self.port, "PostgreSQL started");
        Ok(())
    }

    async fn spawn_owned(
        &self,
        generation: &Arc<ProcessGeneration>,
        native_password: Option<&str>,
        store: Option<&crate::durable::PgDurableStore>,
    ) -> Result<(tokio::process::ChildStdout, tokio::process::ChildStderr), PgError> {
        let mut command = Command::new(self.pg_bin.join("postgres"));
        command.args(["-D", &self.data_dir.to_string_lossy()]);
        if let Some(password) = native_password {
            command.env("PGPASSWORD", password);
        } else {
            command.env_remove("PGPASSWORD");
        }
        let launch = match store {
            Some(store) => Some(
                store
                    .postmaster_launch(
                        crate::owned_command::helper_receipt::PostmasterIntent::new(
                            generation.id,
                            &self.data_dir,
                            &command,
                        )
                        .map_err(|error| PgError::Process(error.to_string()))?,
                    )
                    .await?,
            ),
            None => None,
        };
        // Register before awaiting durable selection so cancellation retains the
        // undispatched supervisor and its actual STOP/ECHILD responsibility.
        let (streams, selected) = {
            let _cleanup = generation.cleanup_lock.lock().unwrap();
            if self.launches_closed.load(Ordering::Acquire)
                || generation.cleanup_result.lock().unwrap().is_some()
            {
                return Err(PgError::Process(
                    "owned PostgreSQL generation is closed".into(),
                ));
            }
            let mut process = match &launch {
                Some(launch) => OwnedProcess::spawn_postmaster(&command, launch)?,
                None => OwnedProcess::spawn(&command)?,
            };
            let streams = process.take_output();
            let selected = process.postmaster_selection().cloned();
            *generation.child.lock().unwrap() = Some(process);
            (streams, selected)
        };
        if let Some(store) = store {
            store
                .select_postmaster(selected.clone().expect("selected postmaster"))
                .await?;
            launch
                .as_ref()
                .unwrap()
                .intent
                .revalidate(&command)
                .map_err(|error| PgError::Process(error.to_string()))?;
        }
        let _cleanup = generation.cleanup_lock.lock().unwrap();
        if self.launches_closed.load(Ordering::Acquire)
            || generation.cleanup_result.lock().unwrap().is_some()
        {
            return Err(PgError::Process("postmaster cancelled before START".into()));
        }
        let owned = generation.child.lock().unwrap();
        let process = owned
            .as_ref()
            .ok_or_else(|| PgError::Process("postmaster retired before START".into()))?;
        if process.postmaster_selection() != selected.as_ref() {
            return Err(PgError::Process(
                "postmaster selection changed before START".into(),
            ));
        }
        process.start()?;
        Ok(streams)
    }

    /// Wait for PG to accept connections.
    async fn wait_ready(&self, generation: &Arc<ProcessGeneration>) -> Result<(), PgError> {
        let mut last_probe_error = None;
        for i in 0..480 {
            let output = tokio::time::timeout(
                std::time::Duration::from_secs(10),
                generation
                    .helpers
                    .output(Command::new(self.pg_bin.join("pg_isready")).args([
                        "-h",
                        &self.socket_dir().to_string_lossy(),
                        "-p",
                        &self.port.to_string(),
                        "-d",
                        "postgres",
                    ])),
            )
            .await
            .map_err(|_| PgError::Process("pg_isready command timeout".into()))??;

            if output.status.success() {
                // Readiness must belong to our launch, not another server using
                // this socket. Keep the backend alive while validating its parent.
                let timeout = std::time::Duration::from_secs(10);
                let (client, connection) = match tokio::time::timeout(timeout, self.connect()).await
                {
                    Ok(Ok(connection)) => connection,
                    Ok(Err(error)) => {
                        last_probe_error = Some(error.to_string());
                        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                        continue;
                    }
                    Err(_) => {
                        last_probe_error = Some("postmaster connection timeout".into());
                        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                        continue;
                    }
                };
                let backend = match tokio::time::timeout(
                    timeout,
                    client.query_one("SELECT pg_backend_pid()", &[]),
                )
                .await
                {
                    Ok(Ok(row)) => row,
                    Ok(Err(error)) => {
                        last_probe_error = Some(format!("identify postmaster: {error}"));
                        drop(client);
                        connection.abort();
                        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                        continue;
                    }
                    Err(_) => {
                        last_probe_error = Some("postmaster identity timeout".into());
                        drop(client);
                        connection.abort();
                        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                        continue;
                    }
                };
                let backend: i32 = backend.get(0);
                let bound = generation
                    .child
                    .lock()
                    .unwrap()
                    .as_mut()
                    .ok_or_else(|| PgError::Process("PostgreSQL ownership lost".into()))?
                    .bind_postmaster(backend as u32);
                drop(client);
                connection.abort();
                bound?;
                tracing::debug!(attempt = i, "pg_isready: accepting connections");
                return Ok(());
            }
            if !self.is_running().await {
                return Err(PgError::Process(
                    "PostgreSQL exited before becoming ready".into(),
                ));
            }
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        }
        Err(PgError::Process(format!(
            "pg_isready timeout (120s){}",
            last_probe_error
                .as_deref()
                .map(|error| format!(": {error}"))
                .unwrap_or_default()
        )))
    }

    /// Stop PostgreSQL (fast mode).
    pub async fn stop(&self) -> Result<(), PgError> {
        let _lifecycle = self.lifecycle().await;
        let result = self.finish_owned().await;
        if result.is_ok() {
            tracing::info!(port = self.port, "PostgreSQL stopped");
        }
        result
    }

    pub async fn is_running(&self) -> bool {
        self.generation().running()
    }

    pub async fn process_state(&self) -> PgProcessState {
        *self.generation().state.lock().unwrap()
    }

    /// Promote standby to primary.
    pub async fn promote(&self) -> Result<(), PgError> {
        let generation = self.generation();
        let output = self
            .command_output(
                &generation,
                Command::new(self.pg_bin.join("pg_ctl")).args([
                    "promote",
                    "-D",
                    &self.data_dir.to_string_lossy(),
                    "-w",
                    "-t",
                    "60",
                ]),
                Some((std::time::Duration::from_secs(60), "pg_ctl promote")),
            )
            .await?;

        if !output.status.success() {
            return Err(PgError::command_failed("pg_ctl promote failed", &output)
                .with_generation(generation.id));
        }

        tracing::info!(port = self.port, "PostgreSQL promoted to primary");
        Ok(())
    }

    /// Run pg_basebackup from a source to initialize this replica.
    pub async fn base_backup(&self, source_host: &str, source_port: u16) -> Result<(), PgError> {
        let generation = self.generation();
        let output = self
            .command_output(
                &generation,
                Command::new(self.pg_bin.join("pg_basebackup")).args([
                    "-D",
                    &self.data_dir.to_string_lossy(),
                    "-h",
                    source_host,
                    "-p",
                    &source_port.to_string(),
                    "-U",
                    &whoami::username().unwrap_or_else(|_| "postgres".to_string()),
                    "-Fp",
                    "-Xs",
                    "-c",
                    "fast",
                    "-R", // creates standby.signal + primary_conninfo
                ]),
                None,
            )
            .await?;

        if !output.status.success() {
            return Err(PgError::command_failed("pg_basebackup failed", &output)
                .with_generation(generation.id));
        }

        tracing::info!(
            port = self.port,
            source = format!("{source_host}:{source_port}"),
            "pg_basebackup complete"
        );
        Ok(())
    }

    pub(crate) async fn public_base_backup(
        &self,
        credential: &crate::adapter::public_transport::NativeCredential,
    ) -> Result<(), PgError> {
        let conninfo = format!(
            "{} password={}",
            credential
                .conninfo()
                .map_err(|error| PgError::Configuration(error.to_string()))?,
            credential.secret
        );
        let helper_conninfo = credential
            .helper_conninfo()
            .map_err(|error| PgError::Configuration(error.to_string()))?;
        let generation = self.generation();
        let output = self
            .command_output(
                &generation,
                Command::new(self.pg_bin.join("pg_basebackup"))
                    .args([
                        "-D",
                        &self.data_dir.to_string_lossy(),
                        "-d",
                        &helper_conninfo,
                        "-Fp",
                        "-Xs",
                        "-c",
                        "fast",
                        "-w",
                        "--no-clean",
                    ])
                    .env("PGPASSWORD", &credential.secret),
                Some((std::time::Duration::from_secs(60), "public pg_basebackup")),
            )
            .await?;
        if !output.status.success() {
            // Connection credentials must never appear in command diagnostics.
            let secret = conninfo
                .split_whitespace()
                .find_map(|field| field.strip_prefix("password="))
                .unwrap_or(&conninfo);
            let diagnostic = String::from_utf8_lossy(&output.stderr).replace(secret, "[redacted]");
            return Err(
                PgError::Process(format!("public pg_basebackup failed: {diagnostic}"))
                    .with_generation(generation.id),
            );
        }
        Ok(())
    }

    /// Run pg_rewind to rejoin as standby after demotion.
    pub async fn rewind(&self, source_host: &str, source_port: u16) -> Result<(), PgError> {
        let source_conn = format!("host={source_host} port={source_port} dbname=postgres");

        let generation = self.generation();
        let output = self
            .command_output(
                &generation,
                Command::new(self.pg_bin.join("pg_rewind")).args([
                    "--target-pgdata",
                    &self.data_dir.to_string_lossy(),
                    "--source-server",
                    &source_conn,
                ]),
                None,
            )
            .await?;

        if !output.status.success() {
            return Err(
                PgError::command_failed("pg_rewind failed", &output).with_generation(generation.id)
            );
        }

        tracing::info!(port = self.port, "pg_rewind complete");
        Ok(())
    }

    pub(crate) async fn prepare_native_build(&self) -> Result<(), PgError> {
        let _lifecycle = self.lifecycle().await;
        let expected_abort = self.abort_generation.load(Ordering::Acquire);
        self.finish_owned().await?;
        self.replace_retired(expected_abort)?;
        Ok(())
    }

    pub(crate) fn repair_candidate(&self, path: PathBuf) -> Self {
        let candidate = Self::new(path, self.pg_bin.clone(), self.port);
        *candidate.helper_store.lock().unwrap() = self.helper_store.lock().unwrap().clone();
        candidate
    }

    pub(crate) async fn clone_stopped_repair_candidate(
        &self,
        destination: &Path,
        candidate_nonce: &[u8; 16],
        valid: crate::adapter::public_transport::GatewayValidity,
        cancellation: CancellationToken,
    ) -> Result<(), PgError> {
        let _lifecycle = self.lifecycle().await;
        let destination_present = match std::fs::symlink_metadata(destination) {
            Ok(_) => true,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
            Err(error) => return Err(PgError::Process(error.to_string())),
        };
        if self.process_state().await != PgProcessState::Stopped
            || self.is_running().await
            || self.data_dir.join("postmaster.pid").exists()
            || destination_present
        {
            return Err(PgError::Process(
                "repair clone requires exclusive stopped trees".into(),
            ));
        }
        let generation = self.generation();
        let mut command = Command::new("cp");
        command
            .arg("-R")
            .arg("--preserve=mode,timestamps")
            .arg("--")
            .arg(&self.data_dir)
            .arg(destination);
        let store = self.native_helper_store()?.ok_or_else(|| {
            PgError::Process("native repair clone requires selected native3 ownership".into())
        })?;
        let intent = crate::owned_command::helper_receipt::HelperIntent::new(
            generation.id,
            *candidate_nonce,
            &self.data_dir,
            destination,
            &command,
        )
        .map_err(|error| PgError::Process(error.to_string()))?;
        let output = {
            let operation = generation
                .helpers
                .output_selected(&mut command, &store, intent);
            tokio::pin!(operation);
            // Selection can hold the native state lock across durable IO.
            // Keep polling it while the lease callback reads that same state.
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => None,
                result = &mut operation => Some(result),
                _ = helper_lease_invalid(&valid, &cancellation) => None,
            }
        };
        let Some(output) = output else {
            let cleanup = generation.helpers.capture().terminate();
            return Err(PgError::Process("owned repair clone cancelled".into())
                .with_cleanup(cleanup)
                .with_generation(generation.id));
        };
        let output = self.complete_generation(&generation, output)?;
        if !output.status.success() {
            return Err(PgError::Process("owned stopped repair clone failed".into()));
        }
        self.complete_generation(&generation, Ok(()))
    }

    // This is only the owned helper boundary. The native repair owner must prove
    // selected two-ended authority, certified ancestry and a faithful stopped copy,
    // and must retain both trees; success does not install or admit the candidate.
    pub(crate) async fn rewind_owned_candidate(
        &self,
        retained: &Self,
        candidate_nonce: &[u8; 16],
        credential: &crate::adapter::public_transport::NativeCredential,
        valid: crate::adapter::public_transport::GatewayValidity,
        cancellation: CancellationToken,
    ) -> Result<(), PgError> {
        use crate::adapter::public_transport::{
            check_rewind_candidate_layout, rewind_candidate_lease_valid,
        };
        use std::os::unix::fs::MetadataExt;

        let conninfo = credential
            .rewind_conninfo()
            .map_err(|error| PgError::Configuration(error.to_string()))?;
        let nonce: String = candidate_nonce.iter().map(|b| format!("{b:02x}")).collect();
        let name = retained
            .data_dir
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| PgError::Configuration("invalid retained PGDATA name".into()))?;
        let expected = retained.data_dir.with_file_name(format!("{name}-{nonce}"));
        if self.data_dir != expected || self.data_dir == retained.data_dir {
            return Err(PgError::Configuration(
                "not the exact retained sibling candidate".into(),
            ));
        }
        let _candidate_lifecycle = self.lifecycle().await;
        let _retained_lifecycle = retained.lifecycle().await;
        let generation = self.generation();
        let original_generation = retained.generation();
        for owner in [self, retained] {
            if owner.process_state().await != PgProcessState::Stopped
                || owner.is_running().await
                || owner.data_dir.join("postmaster.pid").exists()
            {
                return Err(PgError::Process(
                    "rewind requires stopped owned trees".into(),
                ));
            }
            check_rewind_candidate_layout(owner.data_dir.clone())
                .map_err(|error| PgError::Configuration(error.to_string()))?;
        }
        let candidate_root = std::fs::symlink_metadata(&self.data_dir)
            .map_err(|error| PgError::Process(error.to_string()))?;
        let original_root = std::fs::symlink_metadata(&retained.data_dir)
            .map_err(|error| PgError::Process(error.to_string()))?;
        if candidate_root.dev() != original_root.dev()
            || candidate_root.ino() == original_root.ino()
        {
            return Err(PgError::Configuration(
                "candidate is not a distinct same-volume tree".into(),
            ));
        }
        let read_control = |root: &Path| -> Result<Vec<u8>, PgError> {
            let path = root.join("global/pg_control");
            let metadata = std::fs::symlink_metadata(&path)
                .map_err(|error| PgError::Process(error.to_string()))?;
            if !metadata.is_file() || metadata.len() != 8192 || metadata.nlink() != 1 {
                return Err(PgError::Configuration(
                    "unsafe retained control file".into(),
                ));
            }
            std::fs::read(path).map_err(|error| PgError::Process(error.to_string()))
        };
        let original_control = read_control(&retained.data_dir)?;
        if read_control(&self.data_dir)? != original_control {
            return Err(PgError::Configuration(
                "candidate control differs from stopped retained tree".into(),
            ));
        }
        let control = self
            .command_output(
                &generation,
                Command::new(self.pg_bin.join("pg_controldata"))
                    .arg(&self.data_dir)
                    .env("LC_ALL", "C"),
                Some((
                    std::time::Duration::from_secs(2),
                    "candidate pg_controldata",
                )),
            )
            .await?;
        let text = String::from_utf8_lossy(&control.stdout);
        let field = |key| {
            text.lines()
                .find_map(|line| line.strip_prefix(key))
                .map(str::trim)
        };
        if !control.status.success()
            || field("Database cluster state:") != Some("shut down")
            || field("Data page checksum version:") != Some("1")
            || field("Latest checkpoint's full_page_writes:") != Some("on")
        {
            return Err(PgError::Process(
                "candidate is not a checksum/full-page clean primary copy".into(),
            ));
        }
        if !rewind_candidate_lease_valid(&valid, &cancellation).await {
            return Err(PgError::Process(
                "candidate lease revoked before rewind".into(),
            ));
        }
        let mut command = Command::new(self.pg_bin.join("pg_rewind"));
        command
            .arg("--target-pgdata")
            .arg(&self.data_dir)
            .arg("--source-server")
            .arg(conninfo)
            .arg("--no-ensure-shutdown")
            .env("PGPASSWORD", &credential.secret)
            .env("LC_ALL", "C");
        let store = retained.native_helper_store()?.ok_or_else(|| {
            PgError::Process("native candidate rewind requires selected native3 ownership".into())
        })?;
        let intent = crate::owned_command::helper_receipt::HelperIntent::new(
            original_generation.id,
            *candidate_nonce,
            &retained.data_dir,
            &self.data_dir,
            &command,
        )
        .map_err(|error| PgError::Process(error.to_string()))?;
        let output = {
            let operation = generation
                .helpers
                .output_selected(&mut command, &store, intent);
            tokio::pin!(operation);
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => None,
                result = &mut operation => Some(result),
                _ = helper_lease_invalid(&valid, &cancellation) => None,
            }
        };
        let Some(output) = output else {
            // The dropped command future has joined its exact child tree. Keep
            // any retained reap failure sticky in the generation owner.
            let cleanup = generation.helpers.capture().terminate();
            return Err(PgError::Process("candidate rewind cancelled".into())
                .with_cleanup(cleanup)
                .with_generation(generation.id));
        };
        let output = self.complete_generation(&generation, output)?;
        #[cfg(test)]
        eprintln!(
            "owned normal pg_rewind exit={}; stdout={}; stderr={}",
            output.status,
            String::from_utf8_lossy(&output.stdout).replace(&credential.secret, "[redacted]"),
            String::from_utf8_lossy(&output.stderr).replace(&credential.secret, "[redacted]")
        );
        if !output.status.success() {
            let diagnostic =
                String::from_utf8_lossy(&output.stderr).replace(&credential.secret, "[redacted]");
            return Err(PgError::Process(format!(
                "owned candidate pg_rewind failed: {diagnostic}"
            ))
            .with_generation(generation.id));
        }
        if !rewind_candidate_lease_valid(&valid, &cancellation).await
            || !self.generation_is_current(&GenerationLease(generation.clone()))
            || !retained.generation_is_current(&GenerationLease(original_generation))
        {
            return Err(PgError::Process(
                "candidate scope changed during rewind".into(),
            ));
        }
        for (root, pinned) in [
            (&self.data_dir, candidate_root),
            (&retained.data_dir, original_root),
        ] {
            let current = std::fs::symlink_metadata(root)
                .map_err(|error| PgError::Process(error.to_string()))?;
            if !current.is_dir() || current.dev() != pinned.dev() || current.ino() != pinned.ino() {
                return Err(PgError::Process("rewind tree identity changed".into()));
            }
        }
        if read_control(&retained.data_dir)? != original_control {
            return Err(PgError::Process("original retained control changed".into()));
        }
        self.complete_generation(&generation, Ok(()))
    }

    pub(crate) async fn clear_pgdata(&self) -> Result<(), PgError> {
        let _lifecycle = self.lifecycle().await;
        let generation = self.generation();
        if generation.running() {
            return Err(
                PgError::Process("cannot clear running PostgreSQL data".into())
                    .with_generation(generation.id),
            );
        }
        let executable = crate::process_supervisor::executable().map_err(|error| {
            PgError::Process(format!("locate PGDATA deletion helper: {error}"))
                .with_generation(generation.id)
        })?;
        let mut command = Command::new(executable);
        command
            .arg(crate::process_supervisor::CLEAR_PGDATA_ARGUMENT)
            .arg(&self.data_dir);
        #[cfg(test)]
        if let Some((entry, marker)) = self.clear_pgdata_hook.lock().unwrap().take() {
            command.arg(entry).arg(marker);
        }
        let output = self.command_output(&generation, &mut command, None).await?;
        if !output.status.success() {
            return Err(PgError::command_failed("clear PGDATA failed", &output)
                .with_generation(generation.id));
        }
        Ok(())
    }

    pub(crate) async fn rewind_for_build(
        &self,
        source_host: &str,
        source_port: u16,
    ) -> Result<bool, PgError> {
        let generation = self.generation();
        let output = self
            .command_output(
                &generation,
                Command::new(self.pg_bin.join("pg_rewind"))
                    .arg("--target-pgdata")
                    .arg(&self.data_dir)
                    .arg("--source-server")
                    .arg(format!(
                        "host={source_host} port={source_port} dbname=postgres user=kuberic_rewind"
                    ))
                    .arg("--no-ensure-shutdown"),
                None,
            )
            .await?;
        if !output.status.success() {
            tracing::warn!(error = %PgError::command_failed("pg_rewind requires fresh backup", &output), "native rewind fallback");
            return Ok(false);
        }
        Ok(true)
    }

    /// Connect to the local PG instance via UDS.
    pub async fn connect(&self) -> Result<(tokio_postgres::Client, PgTask), PgError> {
        #[cfg(all(test, feature = "testing"))]
        let connection = self.connect_owned(None);
        #[cfg(not(all(test, feature = "testing")))]
        let connection = self.connect_owned();
        connection.await
    }

    async fn connect_owned(
        &self,
        #[cfg(all(test, feature = "testing"))] exit_gate: Option<Arc<DriverExitGate>>,
    ) -> Result<(tokio_postgres::Client, PgTask), PgError> {
        let generation = self.generation();
        let (client, connection) =
            tokio_postgres::connect(&self.connection_string(), tokio_postgres::NoTls)
                .await
                .map_err(|e| {
                    PgError::Connection(format!("connect to {}: {e}", self.connection_string()))
                        .with_generation(generation.id)
                })?;

        let _cleanup = generation.cleanup_lock.lock().unwrap();
        self.complete_generation(&generation, Ok(()))?;
        let shutdown = generation.shutdown.clone();
        let handle = tokio::spawn(async move {
            tokio::select! {
                biased;
                _ = shutdown.cancelled() => {},
                result = connection => if let Err(error) = result {
                    tracing::debug!(%error, "PG connection closed");
                },
            }
            #[cfg(all(test, feature = "testing"))]
            if let Some(gate) = exit_gate {
                gate.entered.notify_one();
                gate.release.notified().await;
            }
        });
        Ok((
            client,
            generation.tasks.retain(generation.id, false, handle),
        ))
    }

    pub async fn connect_application(&self) -> Result<(tokio_postgres::Client, PgTask), PgError> {
        let generation = self.generation();
        let connection_string = self.application_connection_string();
        let (client, connection) =
            tokio_postgres::connect(&connection_string, tokio_postgres::NoTls)
                .await
                .map_err(|error| {
                    PgError::Connection(format!("connect to {connection_string}: {error}"))
                        .with_generation(generation.id)
                })?;
        let _cleanup = generation.cleanup_lock.lock().unwrap();
        self.complete_generation(&generation, Ok(()))?;
        let shutdown = generation.shutdown.clone();
        let handle = tokio::spawn(async move {
            tokio::select! {
                biased;
                _ = shutdown.cancelled() => {},
                result = connection => if let Err(error) = result {
                    tracing::debug!(%error, "PG application connection closed");
                },
            }
        });
        Ok((
            client,
            generation.tasks.retain(generation.id, false, handle),
        ))
    }

    pub(crate) async fn join_tasks(&self) -> Result<(), PgError> {
        self.generation().tasks.join(None).await
    }

    #[cfg(all(test, feature = "testing"))]
    pub(crate) async fn connect_with_driver_exit(
        &self,
        gate: Arc<DriverExitGate>,
    ) -> Result<(tokio_postgres::Client, PgTask), PgError> {
        self.connect_owned(Some(gate)).await
    }

    #[cfg(all(test, feature = "testing"))]
    pub(crate) async fn lose_monitor(&self) {
        self.generation()
            .tasks
            .lose_monitor(self.generation_id())
            .await;
    }

    async fn finish_owned(&self) -> Result<(), PgError> {
        self.retire_owned(true).await
    }

    async fn retire_owned(&self, publish_stopped: bool) -> Result<(), PgError> {
        let generation = self.generation();
        self.retire_generation(generation, publish_stopped).await
    }

    async fn retire_generation(
        &self,
        generation: Arc<ProcessGeneration>,
        publish_stopped: bool,
    ) -> Result<(), PgError> {
        let previous = *generation.state.lock().unwrap();
        *generation.state.lock().unwrap() = PgProcessState::Stopping;
        let ticket = Arc::new(CleanupTicket {
            phase: AtomicU8::new(0),
            commands: generation.helpers.capture(),
            generation: generation.clone(),
            previous,
            launches_closed: self.launches_closed.clone(),
        });
        let waiter = CleanupWaiter(ticket.clone());
        let data_dir = self.data_dir.clone();
        let current = self.current.clone();
        let next = self.next_generation.clone();
        let closed = self.launches_closed.clone();
        let lifecycle = self.lifecycle_lock.clone();
        let retired = generation.clone();
        #[cfg(feature = "testing")]
        let hook = self.cleanup_hook.lock().unwrap().take();
        let (completed, outcome) = tokio::sync::watch::channel(None);
        let task = tokio::task::spawn_blocking(move || {
            if ticket
                .phase
                .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
            {
                completed.send_replace(Some(Ok(())));
                return;
            }
            #[cfg(feature = "testing")]
            if let Some(hook) = &hook {
                hook.entered.notify_one();
                let _ = hook.release.recv();
            }
            let mut result = retired
                .retire(&ticket.commands, &data_dir)
                .map_err(|error| error.with_generation(retired.id));
            if result.is_ok()
                && publish_stopped
                && let Ok(_lifecycle) = lifecycle.try_lock()
            {
                result = publish_stopped_generation(&current, &next, &closed, Some(&retired));
            }
            #[cfg(feature = "testing")]
            if let Some(hook) = hook {
                hook.finished.notify_one();
            }
            completed.send_replace(Some(result));
        });
        generation.tasks.retain(generation.id, false, task).await?;
        let mut result = outcome
            .borrow()
            .clone()
            .ok_or_else(|| PgError::Process("PostgreSQL cleanup result lost".into()))?;
        let tasks = generation.tasks.join(Some(generation.id)).await;
        *generation.tasks_result.lock().unwrap() = Some(tasks.clone());
        if let Err(error) = tasks {
            result = Err(error.with_cleanup(result));
        }
        // A live caller already owns lifecycle_lock. A detached completion may
        // publish only after acquiring it, so fault acknowledgement and process
        // replacement cannot interleave across the generation check.
        if result.is_ok() && publish_stopped {
            publish_stopped_generation(
                &self.current,
                &self.next_generation,
                &self.launches_closed,
                Some(&generation),
            )?;
        }
        drop(waiter);
        result
    }

    pub(crate) fn abort_owned(&self) -> Result<(), PgError> {
        let (generation, advanced) = {
            let current = self.current.lock().unwrap();
            let advanced = increment_generation(&self.abort_generation);
            self.launches_closed.store(true, Ordering::Release);
            (current.clone(), advanced)
        };
        let commands = generation.helpers.capture();
        let cleanup = generation.retire(&commands, &self.data_dir);
        match advanced {
            Some(_) => cleanup,
            None => {
                Err(PgError::GenerationExhausted("process abort epoch".into())
                    .with_cleanup(cleanup))
            }
        }
    }

    fn abort_generation(&self, generation: &Arc<ProcessGeneration>) -> Option<Result<(), PgError>> {
        let advanced = {
            let current = self.current.lock().unwrap();
            if !Arc::ptr_eq(&current, generation) {
                return None;
            }
            let advanced = increment_generation(&self.abort_generation);
            self.launches_closed.store(true, Ordering::Release);
            advanced
        };
        let commands = generation.helpers.capture();
        let cleanup = generation.retire(&commands, &self.data_dir);
        Some(match advanced {
            Some(_) => cleanup,
            None => {
                Err(PgError::GenerationExhausted("process abort epoch".into())
                    .with_cleanup(cleanup))
            }
        })
    }
}

impl Drop for PgInstanceManager {
    fn drop(&mut self) {
        if let Err(error) = self.abort_owned() {
            tracing::error!(%error, "PostgreSQL drop cleanup failed");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{TestDataDir, find_pg_bin};
    use kuberic_runtime::protocol::types::FaultType;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[tokio::test]
    async fn granted_primary_role_mismatch_reports_permanent_before_returning_error() {
        let root = TestDataDir::new("role-mismatch-current");
        let instance = PgInstanceManager::new(root.path().join("pgdata"), find_pg_bin(), 1);
        let access_generation = instance.advance_access_generation().unwrap();
        let reports = Arc::new(AtomicUsize::new(0));
        let observed = reports.clone();
        let result = instance
            .handle_error(
                PgError::Process("primary role mismatch".into())
                    .with_generation(instance.generation_id()),
                Some(access_generation),
                move |fault, _| {
                    let observed = observed.clone();
                    async move {
                        assert_eq!(fault, FaultType::Permanent);
                        observed.fetch_add(1, Ordering::SeqCst);
                        kuberic_runtime::RuntimeError::Application(
                            "permanent mismatch reported".into(),
                        )
                    }
                },
            )
            .await;
        assert!(matches!(
            result,
            kuberic_runtime::RuntimeError::Application(ref message)
                if message == "permanent mismatch reported"
        ));
        assert_eq!(reports.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn retired_role_mismatch_cannot_fault_successor() {
        let root = TestDataDir::new("role-mismatch-retired");
        let instance = PgInstanceManager::new(root.path().join("pgdata"), find_pg_bin(), 1);
        let retired = instance.generation_id();
        instance.prepare_native_build().await.unwrap();
        let successor = instance.generation_id();
        assert!(successor > retired);
        let reports = Arc::new(AtomicUsize::new(0));
        let observed = reports.clone();
        let result = instance
            .handle_error(
                PgError::Process("retired role mismatch".into()).with_generation(retired),
                None,
                move |_, _| {
                    let observed = observed.clone();
                    async move {
                        observed.fetch_add(1, Ordering::SeqCst);
                        kuberic_runtime::RuntimeError::Application(
                            "retired fault was reported".into(),
                        )
                    }
                },
            )
            .await;
        assert!(matches!(
            result,
            kuberic_runtime::RuntimeError::OperationCancelled
        ));
        assert_eq!(reports.load(Ordering::SeqCst), 0);
        assert_eq!(instance.generation_id(), successor);
    }
}

#[derive(Debug, Clone, thiserror::Error)]
pub enum PgError {
    #[error("generation exhausted: {0}")]
    GenerationExhausted(String),
    #[error("{source} [originating process generation {generation}]")]
    Generation {
        generation: u64,
        source: Box<PgError>,
    },
    #[error("operation timeout: {0}")]
    Timeout(String),
    #[error("process error: {0}")]
    Process(String),
    #[error("connection error: {0}")]
    Connection(String),
    #[error("query error: {0}")]
    Query(String),
    #[error("configuration error: {0}")]
    Configuration(String),
}

impl PgError {
    pub(crate) fn with_generation(self, generation: u64) -> Self {
        if matches!(self, Self::Generation { .. }) {
            self
        } else {
            Self::Generation {
                generation,
                source: Box::new(self),
            }
        }
    }

    fn origin(&self) -> Option<u64> {
        match self {
            Self::Generation { generation, .. } => Some(*generation),
            _ => None,
        }
    }

    pub fn is_timeout(&self) -> bool {
        match self {
            Self::Generation { source, .. } => source.is_timeout(),
            Self::Timeout(_) => true,
            _ => false,
        }
    }
    fn command_failed(context: &str, output: &std::process::Output) -> Self {
        Self::Process(format!(
            "{context} ({}); stdout: {}; stderr: {}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        ))
    }

    pub(crate) fn with_cleanup(self, cleanup: Result<(), PgError>) -> Self {
        if let Self::Generation { generation, source } = self {
            return source.with_cleanup(cleanup).with_generation(generation);
        }
        match cleanup {
            Ok(()) => self,
            Err(cleanup) => Self::Process(format!("{self}; owned process cleanup: {cleanup}")),
        }
    }

    pub fn fault_type(&self) -> kuberic_runtime::protocol::types::FaultType {
        match self {
            Self::Generation { source, .. } => source.fault_type(),
            Self::Connection(_) | Self::Query(_) | Self::Timeout(_) => {
                kuberic_runtime::protocol::types::FaultType::Transient
            }
            Self::Process(_) | Self::Configuration(_) | Self::GenerationExhausted(_) => {
                kuberic_runtime::protocol::types::FaultType::Permanent
            }
        }
    }
}
