use std::sync::Arc;
use std::time::Duration;

use kuberic_core::handles::PartitionState;
use kuberic_core::types::Role;

use crate::instance::{PgError, PgInstanceManager};

/// Polls PostgreSQL for replication status and updates PartitionState.
///
/// On the primary, queries `pg_current_wal_lsn()` and `pg_stat_replication`
/// to compute `current_progress` and `committed_lsn`.
///
/// On secondaries, queries `pg_last_wal_receive_lsn()` for self-reported
/// progress used by the operator for failover candidate selection.
pub struct PgMonitor {
    instance: Arc<PgInstanceManager>,
    state: Arc<PartitionState>,
    role: tokio::sync::watch::Sender<Role>,
    role_rx: tokio::sync::watch::Receiver<Role>,
}

impl PgMonitor {
    pub fn new(instance: Arc<PgInstanceManager>, state: Arc<PartitionState>) -> Self {
        let (role_tx, role_rx) = tokio::sync::watch::channel(Role::None);
        Self {
            instance,
            state,
            role: role_tx,
            role_rx,
        }
    }

    /// Update the monitored role.
    pub fn set_role(&self, role: Role) {
        let _ = self.role.send(role);
    }

    /// Run the monitor loop. Polls PG every 1 second.
    pub async fn run(&self, token: kuberic_core::types::CancellationToken) {
        let interval = Duration::from_secs(1);
        let mut consecutive_failures: u32 = 0;

        loop {
            tokio::select! {
                _ = token.cancelled() => break,
                _ = tokio::time::sleep(interval) => {
                    match self.poll_status().await {
                        Ok(()) => {
                            consecutive_failures = 0;
                        }
                        Err(e) => {
                            consecutive_failures += 1;
                            if consecutive_failures <= 3 {
                                tracing::warn!(
                                    failures = consecutive_failures,
                                    "PgMonitor poll failed: {}",
                                    e
                                );
                            }
                            // Retain last-known-good values in PartitionState
                        }
                    }
                }
            }
        }
    }

    /// Force an immediate poll. Called before failover candidate selection.
    pub async fn poll_now(&self) -> Result<(), PgError> {
        self.poll_status().await
    }

    async fn poll_status(&self) -> Result<(), PgError> {
        let role = *self.role_rx.borrow();

        match role {
            Role::Primary => self.poll_primary().await,
            Role::ActiveSecondary | Role::IdleSecondary => self.poll_secondary().await,
            _ => Ok(()),
        }
    }

    async fn poll_primary(&self) -> Result<(), PgError> {
        let (client, _conn) = self.instance.connect().await?;

        // current_progress = pg_current_wal_lsn()
        let row = client
            .query_one("SELECT pg_current_wal_lsn()::text", &[])
            .await
            .map_err(|e| PgError::Query(format!("pg_current_wal_lsn: {e}")))?;

        let lsn_str: &str = row.get(0);
        let current_lsn = parse_pg_lsn(lsn_str)?;
        self.state.set_current_progress(current_lsn);

        // committed_lsn from pg_stat_replication
        // For ANY N quorum: Nth-highest flush_lsn
        // For simplicity, use min(flush_lsn) across sync replicas (conservative)
        let rows = client
            .query(
                "SELECT flush_lsn::text FROM pg_stat_replication \
                 WHERE sync_state IN ('sync', 'quorum') \
                 AND flush_lsn IS NOT NULL \
                 ORDER BY flush_lsn ASC \
                 LIMIT 1",
                &[],
            )
            .await
            .map_err(|e| PgError::Query(format!("pg_stat_replication: {e}")))?;

        if let Some(row) = rows.first() {
            let flush_str: &str = row.get(0);
            let committed = parse_pg_lsn(flush_str)?;
            self.state.set_committed_lsn(committed);
        } else {
            // No sync standbys — single replica or async-only.
            // All local writes are committed (no quorum needed).
            self.state.set_committed_lsn(current_lsn);
        }

        Ok(())
    }

    async fn poll_secondary(&self) -> Result<(), PgError> {
        let (client, _conn) = self.instance.connect().await?;

        // pg_last_wal_receive_lsn() — may be NULL if never connected
        let row = client
            .query_one("SELECT pg_last_wal_receive_lsn()::text", &[])
            .await
            .map_err(|e| PgError::Query(format!("pg_last_wal_receive_lsn: {e}")))?;

        let lsn_opt: Option<&str> = row.get(0);
        if let Some(lsn_str) = lsn_opt {
            let lsn = parse_pg_lsn(lsn_str)?;
            self.state.set_current_progress(lsn);
        }
        // If NULL, retain previous value (don't zero out)

        Ok(())
    }
}

/// Parse PostgreSQL LSN text format (e.g., "0/16B3748") to i64.
///
/// PG LSN is a 64-bit WAL byte offset split as "high32/low32" in hex.
pub fn parse_pg_lsn(s: &str) -> Result<i64, PgError> {
    let parts: Vec<&str> = s.split('/').collect();
    if parts.len() != 2 {
        return Err(PgError::Query(format!("invalid LSN format: {s}")));
    }

    let high = u64::from_str_radix(parts[0], 16)
        .map_err(|e| PgError::Query(format!("LSN parse high: {e}")))?;
    let low = u64::from_str_radix(parts[1], 16)
        .map_err(|e| PgError::Query(format!("LSN parse low: {e}")))?;

    Ok(((high << 32) | low) as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_pg_lsn() {
        assert_eq!(parse_pg_lsn("0/0").unwrap(), 0);
        assert_eq!(parse_pg_lsn("0/1").unwrap(), 1);
        assert_eq!(parse_pg_lsn("0/16B3748").unwrap(), 0x16B3748);
        assert_eq!(parse_pg_lsn("1/0").unwrap(), 0x1_0000_0000);
        assert_eq!(
            parse_pg_lsn("FF/FFFFFFFF").unwrap(),
            0xFF_FFFF_FFFF_u64 as i64
        );
    }

    #[test]
    fn test_parse_pg_lsn_invalid() {
        assert!(parse_pg_lsn("invalid").is_err());
        assert!(parse_pg_lsn("").is_err());
        assert!(parse_pg_lsn("0/0/0").is_err());
    }
}
