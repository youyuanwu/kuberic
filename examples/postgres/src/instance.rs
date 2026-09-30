use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as ProcessMutex};

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::{Mutex, mpsc};

use crate::config::PgConfig;
use crate::owned_command::OwnedCommands;
use crate::owned_process::OwnedProcess;

use tokio_util::sync::CancellationToken;

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

type FaultReporter = Arc<dyn Fn(PgProcessFault) + Send + Sync>;

/// Manages a PostgreSQL instance as a child process.
///
/// Wraps pg_ctl, initdb, pg_basebackup, pg_rewind, and the postgres
/// server process. Each instance gets its own data_dir which doubles
/// as the Unix socket directory for isolation in tests.
pub struct PgInstanceManager {
    data_dir: PathBuf,
    pg_bin: PathBuf,
    port: u16,
    child: Arc<ProcessMutex<Option<OwnedProcess>>>,
    helpers: Arc<OwnedCommands>,
    shutdown_error: Arc<ProcessMutex<Option<PgError>>>,
    config: PgConfig,
    /// Cancelled by stop() to signal health monitor to exit quietly.
    shutdown: Mutex<CancellationToken>,
    monitor_tasks: Mutex<Vec<tokio::task::JoinHandle<()>>>,
    process_state: Arc<Mutex<PgProcessState>>,
    run_generation: AtomicU64,
    lifecycle_lock: Mutex<()>,
}

impl PgInstanceManager {
    pub fn new(data_dir: PathBuf, pg_bin: PathBuf, port: u16) -> Self {
        let config = PgConfig::new(port, &data_dir);
        Self {
            data_dir,
            pg_bin,
            port,
            child: Arc::new(ProcessMutex::new(None)),
            helpers: Arc::new(OwnedCommands::default()),
            shutdown_error: Arc::new(ProcessMutex::new(None)),
            config,
            shutdown: Mutex::new(CancellationToken::new()),
            monitor_tasks: Mutex::new(Vec::new()),
            process_state: Arc::new(Mutex::new(PgProcessState::Stopped)),
            run_generation: AtomicU64::new(0),
            lifecycle_lock: Mutex::new(()),
        }
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

    /// Socket directory — same as data_dir for test isolation.
    pub fn socket_dir(&self) -> &Path {
        &self.data_dir
    }

    /// Connection string for local UDS access.
    /// Uses the current OS user (initdb creates a superuser matching the OS user).
    pub fn connection_string(&self) -> String {
        format!(
            "host={} port={} dbname=postgres",
            self.data_dir.display(),
            self.port,
        )
    }

    pub fn application_connection_string(&self) -> String {
        format!(
            "host={} port={} dbname={} user={}",
            self.data_dir.display(),
            self.port,
            crate::access::APPLICATION_DATABASE,
            crate::access::APPLICATION_ROLE,
        )
    }

    /// Initialize a new PG cluster with data checksums.
    pub async fn init_db(&self) -> Result<(), PgError> {
        let output = self
            .helpers
            .output(
                Command::new(self.pg_bin.join("initdb"))
                    .args([
                        "--data-checksums",
                        "-D",
                        &self.data_dir.to_string_lossy(),
                        "--auth=trust",
                        "--no-instructions",
                    ])
                    .env("LC_ALL", "C"),
            )
            .await?;

        if !output.status.success() {
            return Err(PgError::command_failed("initdb failed", &output));
        }

        // Write required config
        self.config.write_initial(&self.data_dir).await?;
        *self.process_state.lock().await = PgProcessState::Initialized;

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
        fault_tx: mpsc::Sender<kuberic_protocol::types::FaultType>,
    ) -> Result<(), PgError> {
        let reporter: FaultReporter = Arc::new(move |fault| {
            let fault = match fault {
                PgProcessFault::Transient => kuberic_protocol::types::FaultType::Transient,
                PgProcessFault::Permanent => kuberic_protocol::types::FaultType::Permanent,
            };
            let fault_tx = fault_tx.clone();
            tokio::spawn(async move {
                let _ = fault_tx.send(fault).await;
            });
        });
        self.start_with_fault_reporter(reporter).await
    }

    pub async fn start_native_with_cancellation(
        self: &Arc<Self>,
        fault_tx: mpsc::Sender<kuberic_protocol::types::FaultType>,
        cancellation: tokio_util::sync::CancellationToken,
    ) -> Result<(), PgError> {
        self.start_native(fault_tx).await?;
        let generation = self.run_generation.load(Ordering::Acquire);
        let instance = Arc::downgrade(self);
        let task = tokio::spawn(async move {
            cancellation.cancelled().await;
            if let Some(instance) = instance.upgrade() {
                let _lifecycle = instance.lifecycle_lock.lock().await;
                if instance.run_generation.load(Ordering::Acquire) == generation {
                    let result = instance.abort_owned();
                    *instance.process_state.lock().await = if result.is_ok() {
                        PgProcessState::Stopped
                    } else {
                        PgProcessState::Faulted
                    };
                }
            }
        });
        self.monitor_tasks.lock().await.push(task);
        Ok(())
    }

    pub async fn control_identity(&self) -> Result<(String, u32), PgError> {
        let output = self
            .helpers
            .output(
                Command::new(self.pg_bin.join("pg_controldata"))
                    .arg(&self.data_dir)
                    .env("LC_ALL", "C"),
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
        let timeline = field("Latest checkpoint's TimeLineID:")?
            .parse()
            .map_err(|_| PgError::Process("invalid control timeline".into()))?;
        Ok((system, timeline))
    }

    async fn start_with_fault_reporter(
        &self,
        fault_reporter: FaultReporter,
    ) -> Result<(), PgError> {
        let _lifecycle = self.lifecycle_lock.lock().await;
        if self.is_running().await {
            return Ok(());
        }
        *self.shutdown_error.lock().unwrap() = None;
        self.finish_owned().await?;
        self.shutdown.lock().await.cancel();
        for task in std::mem::take(&mut *self.monitor_tasks.lock().await) {
            task.abort();
            let _ = task.await;
        }
        self.helpers.reopen()?;
        self.run_generation.fetch_add(1, Ordering::AcqRel);
        *self.process_state.lock().await = PgProcessState::Starting;
        let prepared = async {
            OwnedProcess::check_support()?;
            self.control_identity().await?;
            self.config.write_initial(&self.data_dir).await
        }
        .await;
        if let Err(error) = prepared {
            fault_reporter(PgProcessFault::Permanent);
            *self.process_state.lock().await = PgProcessState::Faulted;
            return Err(error);
        }

        let (stdout, stderr) = match self.spawn_owned() {
            Ok(streams) => streams,
            Err(error) => {
                fault_reporter(PgProcessFault::Permanent);
                *self.process_state.lock().await = PgProcessState::Faulted;
                return Err(error);
            }
        };

        // Forward stdout + stderr through tracing in one task
        let stdout = BufReader::new(stdout);
        let stderr = BufReader::new(stderr);
        let log_task = tokio::spawn(async move {
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
        self.monitor_tasks.lock().await.push(log_task);

        // Fresh shutdown token for this run
        let shutdown = CancellationToken::new();
        *self.shutdown.lock().await = shutdown.clone();

        // Wait for PG to be ready
        if let Err(error) = self.wait_ready().await {
            fault_reporter(PgProcessFault::Permanent);
            let cleanup = self.finish_owned().await;
            for task in std::mem::take(&mut *self.monitor_tasks.lock().await) {
                task.abort();
            }
            *self.process_state.lock().await = PgProcessState::Faulted;
            return Err(error.with_cleanup(cleanup));
        }

        // Observe without reaping: shutdown retains ownership of any descendants.
        {
            let child_mutex = self.child.clone();
            let exit_shutdown = shutdown.clone();
            let exit_fault_reporter = fault_reporter.clone();
            let exit_port = self.port;
            let exit_state = self.process_state.clone();
            let exit_task = tokio::spawn(async move {
                loop {
                    tokio::select! {
                        _ = exit_shutdown.cancelled() => break,
                        _ = tokio::time::sleep(std::time::Duration::from_millis(500)) => {}
                    }
                    let exited = match child_mutex.try_lock() {
                        Ok(guard) => guard.as_ref().map(OwnedProcess::exited),
                        Err(_) => continue,
                    };
                    if let Some(exited) = exited {
                        match exited {
                            Ok(true) => {
                                if !exit_shutdown.is_cancelled() {
                                    tracing::error!(
                                        port = exit_port,
                                        "PostgreSQL exited unexpectedly"
                                    );
                                    exit_fault_reporter(PgProcessFault::Permanent);
                                    *exit_state.lock().await = PgProcessState::Faulted;
                                }
                                break;
                            }
                            Ok(false) => {}
                            Err(e) => {
                                tracing::warn!("process observation error: {}", e);
                            }
                        }
                    } else {
                        break;
                    }
                }
            });
            self.monitor_tasks.lock().await.push(exit_task);
        }

        // Spawn pg_isready health monitor — catches PG hung but process alive
        // (complements the exit monitor above). Exits quietly on shutdown.
        let port = self.port;
        let health_fault_reporter = fault_reporter;
        let data_dir = self.data_dir.clone();
        let pg_bin = self.pg_bin.clone();
        let shutdown = shutdown.clone();
        let health_state = self.process_state.clone();
        let helpers = self.helpers.clone();
        let health_task = tokio::spawn(async move {
            let mut consecutive_failures: u32 = 0;
            loop {
                tokio::select! {
                    _ = shutdown.cancelled() => break,
                    _ = tokio::time::sleep(std::time::Duration::from_secs(2)) => {}
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
                                health_fault_reporter(PgProcessFault::Permanent);
                                *health_state.lock().await = PgProcessState::Faulted;
                            }
                            break;
                        }
                    }
                }
            }
        });
        self.monitor_tasks.lock().await.push(health_task);

        *self.process_state.lock().await = PgProcessState::Running;
        tracing::info!(port = self.port, "PostgreSQL started");
        Ok(())
    }

    fn spawn_owned(
        &self,
    ) -> Result<(tokio::process::ChildStdout, tokio::process::ChildStderr), PgError> {
        // Registration and spawn are indivisible with respect to synchronous abort.
        let mut owned = self.child.lock().unwrap();
        let mut process = OwnedProcess::spawn(
            Command::new(self.pg_bin.join("postgres"))
                .args(["-D", &self.data_dir.to_string_lossy()]),
        )?;
        let streams = process.take_output();
        *owned = Some(process);
        owned.as_mut().unwrap().start()?;
        Ok(streams)
    }

    /// Wait for PG to accept connections.
    async fn wait_ready(&self) -> Result<(), PgError> {
        for i in 0..60 {
            let output = tokio::time::timeout(
                std::time::Duration::from_secs(2),
                self.helpers
                    .output(Command::new(self.pg_bin.join("pg_isready")).args([
                        "-h",
                        &self.data_dir.to_string_lossy(),
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
                let timeout = std::time::Duration::from_secs(2);
                let (client, connection) =
                    tokio::time::timeout(timeout, self.connect())
                        .await
                        .map_err(|_| PgError::Process("postmaster connection timeout".into()))??;
                let backend =
                    tokio::time::timeout(timeout, client.query_one("SELECT pg_backend_pid()", &[]))
                        .await
                        .map_err(|_| PgError::Process("postmaster identity timeout".into()))
                        .and_then(|result| {
                            result.map_err(|error| {
                                PgError::Process(format!("identify postmaster: {error}"))
                            })
                        });
                let bound = backend.and_then(|row| {
                    let backend: i32 = row.get(0);
                    self.child
                        .lock()
                        .unwrap()
                        .as_mut()
                        .ok_or_else(|| PgError::Process("PostgreSQL ownership lost".into()))?
                        .bind_postmaster(backend as u32)
                });
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
        Err(PgError::Process("pg_isready timeout (15s)".into()))
    }

    /// Stop PostgreSQL (fast mode).
    pub async fn stop(&self) -> Result<(), PgError> {
        self.stop_generation(None).await
    }

    async fn stop_generation(&self, expected_generation: Option<u64>) -> Result<(), PgError> {
        let _lifecycle = self.lifecycle_lock.lock().await;
        if expected_generation
            .is_some_and(|expected| self.run_generation.load(Ordering::Acquire) != expected)
        {
            return Ok(());
        }
        self.run_generation.fetch_add(1, Ordering::AcqRel);
        *self.process_state.lock().await = PgProcessState::Stopping;
        // Signal health monitor to exit quietly before stopping PG
        self.shutdown.lock().await.cancel();

        for task in std::mem::take(&mut *self.monitor_tasks.lock().await) {
            task.abort();
            let _ = task.await;
        }
        let result = self.finish_owned().await;
        if result.is_err() {
            *self.process_state.lock().await = PgProcessState::Faulted;
            return result;
        }

        *self.process_state.lock().await = PgProcessState::Stopped;
        tracing::info!(port = self.port, "PostgreSQL stopped");
        Ok(())
    }

    pub async fn is_running(&self) -> bool {
        let child = self.child.lock().unwrap();
        child
            .as_ref()
            .is_some_and(|child| !child.exited().unwrap_or(false))
    }

    pub async fn process_state(&self) -> PgProcessState {
        *self.process_state.lock().await
    }

    /// Promote standby to primary.
    pub async fn promote(&self) -> Result<(), PgError> {
        let output = self
            .helpers
            .output(Command::new(self.pg_bin.join("pg_ctl")).args([
                "promote",
                "-D",
                &self.data_dir.to_string_lossy(),
                "-w",
                "-t",
                "60",
            ]))
            .await?;

        if !output.status.success() {
            return Err(PgError::command_failed("pg_ctl promote failed", &output));
        }

        tracing::info!(port = self.port, "PostgreSQL promoted to primary");
        Ok(())
    }

    /// Run pg_basebackup from a source to initialize this replica.
    pub async fn base_backup(&self, source_host: &str, source_port: u16) -> Result<(), PgError> {
        let output = self
            .helpers
            .output(Command::new(self.pg_bin.join("pg_basebackup")).args([
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
            ]))
            .await?;

        if !output.status.success() {
            return Err(PgError::command_failed("pg_basebackup failed", &output));
        }

        tracing::info!(
            port = self.port,
            source = format!("{source_host}:{source_port}"),
            "pg_basebackup complete"
        );
        Ok(())
    }

    /// Run pg_rewind to rejoin as standby after demotion.
    pub async fn rewind(&self, source_host: &str, source_port: u16) -> Result<(), PgError> {
        let source_conn = format!("host={source_host} port={source_port} dbname=postgres");

        let output = self
            .helpers
            .output(Command::new(self.pg_bin.join("pg_rewind")).args([
                "--target-pgdata",
                &self.data_dir.to_string_lossy(),
                "--source-server",
                &source_conn,
            ]))
            .await?;

        if !output.status.success() {
            return Err(PgError::command_failed("pg_rewind failed", &output));
        }

        tracing::info!(port = self.port, "pg_rewind complete");
        Ok(())
    }

    pub(crate) async fn prepare_native_build(&self) -> Result<(), PgError> {
        self.stop().await?;
        self.helpers.reopen()
    }

    pub(crate) async fn rewind_for_build(
        &self,
        source_host: &str,
        source_port: u16,
    ) -> Result<bool, PgError> {
        let output = self
            .helpers
            .output(
                Command::new(self.pg_bin.join("pg_rewind"))
                    .arg("--target-pgdata")
                    .arg(&self.data_dir)
                    .arg("--source-server")
                    .arg(format!(
                        "host={source_host} port={source_port} dbname=postgres user=kuberic_rewind"
                    ))
                    .arg("--no-ensure-shutdown"),
            )
            .await?;
        if !output.status.success() {
            tracing::warn!(error = %PgError::command_failed("pg_rewind requires fresh backup", &output), "native rewind fallback");
            return Ok(false);
        }
        Ok(true)
    }

    /// Connect to the local PG instance via UDS.
    pub async fn connect(
        &self,
    ) -> Result<(tokio_postgres::Client, tokio::task::JoinHandle<()>), PgError> {
        let (client, connection) =
            tokio_postgres::connect(&self.connection_string(), tokio_postgres::NoTls)
                .await
                .map_err(|e| {
                    PgError::Connection(format!("connect to {}: {e}", self.connection_string()))
                })?;

        let handle = tokio::spawn(async move {
            if let Err(e) = connection.await {
                // Expected during shutdown — PG kills connections on pg_ctl stop
                tracing::debug!("PG connection closed: {}", e);
            }
        });

        Ok((client, handle))
    }

    pub async fn connect_application(
        &self,
    ) -> Result<(tokio_postgres::Client, tokio::task::JoinHandle<()>), PgError> {
        let connection_string = self.application_connection_string();
        let (client, connection) =
            tokio_postgres::connect(&connection_string, tokio_postgres::NoTls)
                .await
                .map_err(|error| {
                    PgError::Connection(format!("connect to {connection_string}: {error}"))
                })?;
        let handle = tokio::spawn(async move {
            if let Err(error) = connection.await {
                tracing::debug!("PG application connection closed: {}", error);
            }
        });
        Ok((client, handle))
    }

    async fn finish_owned(&self) -> Result<(), PgError> {
        let child = self.child.clone();
        let error = self.shutdown_error.clone();
        let data_dir = self.data_dir.clone();
        let helpers = self.helpers.clone();
        tokio::task::spawn_blocking(move || {
            let commands = helpers.terminate(false);
            let postgres = Self::terminate_owned(&child, &error, &data_dir);
            match commands {
                Ok(()) => postgres,
                Err(error) => Err(error.with_cleanup(postgres)),
            }
        })
        .await
        .map_err(|error| PgError::Process(format!("join PostgreSQL cleanup: {error}")))?
    }

    fn terminate_owned(
        child: &ProcessMutex<Option<OwnedProcess>>,
        error: &ProcessMutex<Option<PgError>>,
        data_dir: &Path,
    ) -> Result<(), PgError> {
        let mut child = child.lock().unwrap();
        if let Some(owned) = child.as_mut() {
            let result = owned.stop(data_dir);
            // Even a failed graceful stop can have fully cleaned up. Retain its error
            // across runtime abort / cancellation / application close callers.
            if result.is_ok() {
                child.take();
            }
            if let Err(cleanup) = result {
                *error.lock().unwrap() = Some(cleanup);
            }
        }
        error.lock().unwrap().clone().map_or(Ok(()), Err)
    }

    pub(crate) fn abort_owned(&self) -> Result<(), PgError> {
        if let Ok(shutdown) = self.shutdown.try_lock() {
            shutdown.cancel();
        }
        let helpers = self.helpers.terminate(true);
        let postgres = Self::terminate_owned(&self.child, &self.shutdown_error, &self.data_dir);
        match helpers {
            Ok(()) => postgres,
            Err(error) => Err(error.with_cleanup(postgres)),
        }
    }
}

impl Drop for PgInstanceManager {
    fn drop(&mut self) {
        // Cancel health monitor + process exit monitor
        self.shutdown.get_mut().cancel();
        for task in self.monitor_tasks.get_mut().drain(..) {
            task.abort();
        }
        if let Err(error) = self.abort_owned() {
            tracing::error!(%error, "PostgreSQL drop cleanup failed");
        }
    }
}

#[derive(Debug, Clone, thiserror::Error)]
pub enum PgError {
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
    fn command_failed(context: &str, output: &std::process::Output) -> Self {
        Self::Process(format!(
            "{context} ({}); stdout: {}; stderr: {}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        ))
    }

    pub(crate) fn with_cleanup(self, cleanup: Result<(), PgError>) -> Self {
        match cleanup {
            Ok(()) => self,
            Err(cleanup) => Self::Process(format!("{self}; owned process cleanup: {cleanup}")),
        }
    }

    pub fn fault_type(&self) -> kuberic_protocol::types::FaultType {
        match self {
            Self::Connection(_) | Self::Query(_) => kuberic_protocol::types::FaultType::Transient,
            Self::Process(_) | Self::Configuration(_) => {
                kuberic_protocol::types::FaultType::Permanent
            }
        }
    }
}
