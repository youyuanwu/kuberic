use std::path::{Path, PathBuf};
use std::process::Stdio;

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{Mutex, mpsc};

use kuberic_core::types::FaultType;

/// Manages a PostgreSQL instance as a child process.
///
/// Wraps pg_ctl, initdb, pg_basebackup, pg_rewind, and the postgres
/// server process. Each instance gets its own data_dir which doubles
/// as the Unix socket directory for isolation in tests.
pub struct PgInstanceManager {
    data_dir: PathBuf,
    pg_bin: PathBuf,
    port: u16,
    child: Mutex<Option<Child>>,
}

impl PgInstanceManager {
    pub fn new(data_dir: PathBuf, pg_bin: PathBuf, port: u16) -> Self {
        Self {
            data_dir,
            pg_bin,
            port,
            child: Mutex::new(None),
        }
    }

    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    pub fn port(&self) -> u16 {
        self.port
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

    /// Initialize a new PG cluster with data checksums.
    pub async fn init_db(&self) -> Result<(), PgError> {
        let output = Command::new(self.pg_bin.join("initdb"))
            .args([
                "--data-checksums",
                "-D",
                &self.data_dir.to_string_lossy(),
                "--auth=trust",
                "--no-instructions",
            ])
            .env("LC_ALL", "C")
            .output()
            .await
            .map_err(|e| PgError::Process(format!("initdb spawn: {e}")))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(PgError::Process(format!("initdb failed: {stderr}")));
        }

        // Write required config
        self.write_config().await?;

        tracing::info!(
            data_dir = %self.data_dir.display(),
            port = self.port,
            "initdb complete"
        );
        Ok(())
    }

    /// Write required postgresql.conf settings.
    async fn write_config(&self) -> Result<(), PgError> {
        let conf_path = self.data_dir.join("postgresql.conf");
        let extra = format!(
            "\n\
            # kuberic required settings\n\
            port = {port}\n\
            unix_socket_directories = '{socket_dir}'\n\
            wal_log_hints = on\n\
            hot_standby = on\n\
            synchronous_commit = on\n\
            logging_collector = off\n\
            listen_addresses = '*'\n\
            wal_level = replica\n\
            max_wal_senders = 10\n\
            max_replication_slots = 10\n\
            ",
            port = self.port,
            socket_dir = self.data_dir.display(),
        );

        tokio::fs::write(
            &conf_path,
            format!(
                "{}\n{extra}",
                tokio::fs::read_to_string(&conf_path)
                    .await
                    .unwrap_or_default()
            ),
        )
        .await
        .map_err(|e| PgError::Process(format!("write config: {e}")))?;

        // Allow replication connections in pg_hba.conf
        let hba_path = self.data_dir.join("pg_hba.conf");
        let hba_extra = "\n\
            # kuberic: allow replication connections\n\
            local   replication     all                     trust\n\
            host    replication     all     127.0.0.1/32    trust\n\
            host    replication     all     ::1/128         trust\n\
            host    replication     all     0.0.0.0/0       trust\n\
            host    all             all     0.0.0.0/0       trust\n\
        ";

        let mut hba = tokio::fs::read_to_string(&hba_path)
            .await
            .unwrap_or_default();
        hba.push_str(hba_extra);
        tokio::fs::write(&hba_path, hba)
            .await
            .map_err(|e| PgError::Process(format!("write pg_hba.conf: {e}")))?;

        Ok(())
    }

    /// Start PostgreSQL. Pipes stdout/stderr through tracing.
    /// Spawns a child process monitor that reports fault on unexpected exit.
    pub async fn start(&self, fault_tx: mpsc::Sender<FaultType>) -> Result<(), PgError> {
        {
            let guard = self.child.lock().await;
            if guard.is_some() {
                return Ok(()); // already running
            }
        }

        let mut child = Command::new(self.pg_bin.join("postgres"))
            .args(["-D", &self.data_dir.to_string_lossy()])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| PgError::Process(format!("postgres spawn: {e}")))?;

        // Forward stdout + stderr through tracing in one task
        let stdout = BufReader::new(child.stdout.take().unwrap());
        let stderr = BufReader::new(child.stderr.take().unwrap());
        tokio::spawn(async move {
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

        *self.child.lock().await = Some(child);

        // Wait for PG to be ready
        self.wait_ready().await?;

        // Spawn child process monitor (detect unexpected exit)
        // We need to take the child temporarily to get its id, but
        // the actual wait happens on a separate mechanism.
        let port = self.port;
        let ft = fault_tx.clone();
        let data_dir = self.data_dir.clone();
        let pg_bin = self.pg_bin.clone();
        tokio::spawn(async move {
            // Poll pg_isready to detect crash. The actual child.wait()
            // can't be used because we store &mut child in self.
            // Instead, use pg_isready as a health check loop.
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                let status = Command::new(pg_bin.join("pg_isready"))
                    .args(["-h", &data_dir.to_string_lossy(), "-p", &port.to_string()])
                    .output()
                    .await;
                match status {
                    Ok(output) if output.status.success() => continue,
                    Ok(_) => {
                        tracing::error!(port, "PostgreSQL is not ready (possible crash)");
                        let _ = ft.send(FaultType::Permanent).await;
                        break;
                    }
                    Err(e) => {
                        tracing::error!(port, "pg_isready failed: {}", e);
                        let _ = ft.send(FaultType::Permanent).await;
                        break;
                    }
                }
            }
        });

        tracing::info!(port = self.port, "PostgreSQL started");
        Ok(())
    }

    /// Wait for PG to accept connections.
    async fn wait_ready(&self) -> Result<(), PgError> {
        for i in 0..60 {
            let output = Command::new(self.pg_bin.join("pg_isready"))
                .args([
                    "-h",
                    &self.data_dir.to_string_lossy(),
                    "-p",
                    &self.port.to_string(),
                ])
                .output()
                .await
                .map_err(|e| PgError::Process(format!("pg_isready: {e}")))?;

            if output.status.success() {
                tracing::debug!(attempt = i, "pg_isready: accepting connections");
                return Ok(());
            }
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        }
        Err(PgError::Process("pg_isready timeout (15s)".into()))
    }

    /// Stop PostgreSQL (fast mode).
    pub async fn stop(&self) -> Result<(), PgError> {
        let output = Command::new(self.pg_bin.join("pg_ctl"))
            .args([
                "stop",
                "-D",
                &self.data_dir.to_string_lossy(),
                "-m",
                "fast",
                "-w",
            ])
            .output()
            .await
            .map_err(|e| PgError::Process(format!("pg_ctl stop: {e}")))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            // Not running is OK
            if !stderr.contains("not running") {
                return Err(PgError::Process(format!("pg_ctl stop failed: {stderr}")));
            }
        }

        // Reap child process if we have one
        if let Some(mut child) = self.child.lock().await.take() {
            let _ = child.wait().await;
        }

        tracing::info!(port = self.port, "PostgreSQL stopped");
        Ok(())
    }

    /// Promote standby to primary.
    pub async fn promote(&self) -> Result<(), PgError> {
        let output = Command::new(self.pg_bin.join("pg_ctl"))
            .args([
                "promote",
                "-D",
                &self.data_dir.to_string_lossy(),
                "-w",
                "-t",
                "60",
            ])
            .output()
            .await
            .map_err(|e| PgError::Process(format!("pg_ctl promote: {e}")))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(PgError::Process(format!("pg_ctl promote failed: {stderr}")));
        }

        tracing::info!(port = self.port, "PostgreSQL promoted to primary");
        Ok(())
    }

    /// Run pg_basebackup from a source to initialize this replica.
    pub async fn base_backup(&self, source_host: &str, source_port: u16) -> Result<(), PgError> {
        let output = Command::new(self.pg_bin.join("pg_basebackup"))
            .args([
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
                "-R", // creates standby.signal + primary_conninfo
            ])
            .output()
            .await
            .map_err(|e| PgError::Process(format!("pg_basebackup: {e}")))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(PgError::Process(format!("pg_basebackup failed: {stderr}")));
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

        let output = Command::new(self.pg_bin.join("pg_rewind"))
            .args([
                "--target-pgdata",
                &self.data_dir.to_string_lossy(),
                "--source-server",
                &source_conn,
            ])
            .output()
            .await
            .map_err(|e| PgError::Process(format!("pg_rewind: {e}")))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(PgError::Process(format!("pg_rewind failed: {stderr}")));
        }

        tracing::info!(port = self.port, "pg_rewind complete");
        Ok(())
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
                tracing::error!("PG connection error: {}", e);
            }
        });

        Ok((client, handle))
    }
}

impl Drop for PgInstanceManager {
    fn drop(&mut self) {
        if self.child.get_mut().is_some() {
            // Best-effort stop via pg_ctl (sync, can't await in drop)
            let _ = std::process::Command::new(self.pg_bin.join("pg_ctl"))
                .args([
                    "stop",
                    "-D",
                    &self.data_dir.to_string_lossy(),
                    "-m",
                    "immediate",
                ])
                .output();
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum PgError {
    #[error("process error: {0}")]
    Process(String),
    #[error("connection error: {0}")]
    Connection(String),
    #[error("query error: {0}")]
    Query(String),
}
