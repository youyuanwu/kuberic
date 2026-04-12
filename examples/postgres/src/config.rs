use std::path::Path;

use crate::instance::PgError;

/// All kuberic-required PostgreSQL configuration in one place.
/// Handles initial setup (after initdb) and patching (after pg_basebackup).
pub struct PgConfig {
    pub port: u16,
    pub socket_dir: String,
}

impl PgConfig {
    pub fn new(port: u16, data_dir: &Path) -> Self {
        Self {
            port,
            socket_dir: data_dir.to_string_lossy().to_string(),
        }
    }

    /// Write kuberic settings to postgresql.conf (appended after initdb).
    /// Also writes pg_hba.conf for replication access.
    pub async fn write_initial(&self, data_dir: &Path) -> Result<(), PgError> {
        self.write_postgresql_conf(data_dir).await?;
        self.write_pg_hba_conf(data_dir).await?;
        Ok(())
    }

    /// Patch config after pg_basebackup clone.
    /// Fixes port, socket_dir (copied from primary), and sets
    /// application_name in primary_conninfo for sync standby matching.
    pub async fn patch_after_clone(&self, data_dir: &Path, replica_id: i64) -> Result<(), PgError> {
        self.patch_postgresql_conf(data_dir).await?;
        self.patch_primary_conninfo(data_dir, replica_id).await?;
        Ok(())
    }

    /// Append kuberic-required settings to postgresql.conf.
    async fn write_postgresql_conf(&self, data_dir: &Path) -> Result<(), PgError> {
        let conf_path = data_dir.join("postgresql.conf");
        let existing = tokio::fs::read_to_string(&conf_path)
            .await
            .unwrap_or_default();

        let kuberic_conf = format!(
            r#"
# --- kuberic required settings ---
port = {port}
unix_socket_directories = '{socket_dir}'
wal_log_hints = on
hot_standby = on
synchronous_commit = on
logging_collector = off
listen_addresses = '*'
wal_level = replica
max_wal_senders = 10
max_replication_slots = 10
"#,
            port = self.port,
            socket_dir = self.socket_dir,
        );

        tokio::fs::write(&conf_path, format!("{existing}\n{kuberic_conf}"))
            .await
            .map_err(|e| PgError::Process(format!("write postgresql.conf: {e}")))?;

        Ok(())
    }

    /// Write pg_hba.conf entries for replication + trust auth.
    async fn write_pg_hba_conf(&self, data_dir: &Path) -> Result<(), PgError> {
        let hba_path = data_dir.join("pg_hba.conf");
        let mut content = tokio::fs::read_to_string(&hba_path)
            .await
            .unwrap_or_default();

        content.push_str(
            r#"
# --- kuberic: allow replication and client connections ---
local   replication     all                     trust
host    replication     all     127.0.0.1/32    trust
host    replication     all     ::1/128         trust
host    replication     all     0.0.0.0/0       trust
host    all             all     0.0.0.0/0       trust
"#,
        );

        tokio::fs::write(&hba_path, content)
            .await
            .map_err(|e| PgError::Process(format!("write pg_hba.conf: {e}")))?;

        Ok(())
    }

    /// Fix port and socket_dir in postgresql.conf after pg_basebackup.
    /// pg_basebackup copies the primary's config which has wrong values.
    async fn patch_postgresql_conf(&self, data_dir: &Path) -> Result<(), PgError> {
        let conf_path = data_dir.join("postgresql.conf");
        let content = tokio::fs::read_to_string(&conf_path)
            .await
            .map_err(|e| PgError::Process(format!("read postgresql.conf: {e}")))?;

        let mut new_lines: Vec<String> = content
            .lines()
            .filter(|line| {
                let trimmed = line.trim();
                !trimmed.starts_with("port ")
                    && !trimmed.starts_with("port=")
                    && !trimmed.starts_with("unix_socket_directories")
            })
            .map(String::from)
            .collect();

        new_lines.push(format!("port = {}", self.port));
        new_lines.push(format!("unix_socket_directories = '{}'", self.socket_dir));

        tokio::fs::write(&conf_path, new_lines.join("\n") + "\n")
            .await
            .map_err(|e| PgError::Process(format!("patch postgresql.conf: {e}")))?;

        tracing::debug!(port = self.port, "postgresql.conf patched after clone");
        Ok(())
    }

    /// Set application_name in primary_conninfo (postgresql.auto.conf).
    /// pg_basebackup -R writes primary_conninfo with default application_name.
    /// We need kuberic_{replica_id} for synchronous_standby_names matching.
    async fn patch_primary_conninfo(
        &self,
        data_dir: &Path,
        replica_id: i64,
    ) -> Result<(), PgError> {
        let auto_conf = data_dir.join("postgresql.auto.conf");
        let content = tokio::fs::read_to_string(&auto_conf)
            .await
            .map_err(|e| PgError::Process(format!("read auto.conf: {e}")))?;

        let app_name = format!("kuberic_{replica_id}");
        let new_lines: Vec<String> = content
            .lines()
            .map(|line| {
                if line.contains("primary_conninfo") && !line.contains("application_name") {
                    // Inject application_name before closing quote
                    if let Some(pos) = line.rfind('\'') {
                        let mut modified = line[..pos].to_string();
                        modified.push_str(&format!(" application_name={app_name}'"));
                        return modified;
                    }
                }
                line.to_string()
            })
            .collect();

        tokio::fs::write(&auto_conf, new_lines.join("\n") + "\n")
            .await
            .map_err(|e| PgError::Process(format!("patch auto.conf: {e}")))?;

        tracing::debug!(%app_name, "application_name set in primary_conninfo");
        Ok(())
    }

    /// Rewrite primary_conninfo to point to a new primary.
    /// Used after failover when secondaries need to reconnect.
    pub async fn rewrite_primary_conninfo(
        data_dir: &Path,
        primary_host: &str,
        primary_port: u32,
    ) -> Result<(), PgError> {
        let auto_conf = data_dir.join("postgresql.auto.conf");
        let content = tokio::fs::read_to_string(&auto_conf)
            .await
            .map_err(|e| PgError::Process(format!("read auto.conf: {e}")))?;

        let new_conninfo = format!("host={primary_host} port={primary_port}");
        let new_lines: Vec<String> = content
            .lines()
            .map(|line| {
                if line.contains("primary_conninfo") {
                    // Keep application_name, replace host/port
                    let app_name = line
                        .split("application_name=")
                        .nth(1)
                        .and_then(|s| s.split('\'').next())
                        .unwrap_or("walreceiver");
                    format!("primary_conninfo = '{new_conninfo} application_name={app_name}'")
                } else {
                    line.to_string()
                }
            })
            .collect();

        tokio::fs::write(&auto_conf, new_lines.join("\n") + "\n")
            .await
            .map_err(|e| PgError::Process(format!("rewrite primary_conninfo: {e}")))?;

        tracing::debug!(%primary_host, primary_port, "primary_conninfo rewritten");
        Ok(())
    }
}
