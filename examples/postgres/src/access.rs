use crate::config::PgConfig;
use crate::instance::{PgError, PgInstanceManager};
use std::sync::atomic::Ordering;
use std::time::Duration;

pub const APPLICATION_ROLE: &str = "kuberic_app";
pub const APPLICATION_DATABASE: &str = "kuberic";
pub(crate) const CLOSED: u8 = 0;
const GRANTING: u8 = 1;
const GRANTED: u8 = 2;

pub struct PgAccessController<'a> {
    instance: &'a PgInstanceManager,
}

impl<'a> PgAccessController<'a> {
    pub fn new(instance: &'a PgInstanceManager) -> Self {
        Self { instance }
    }

    pub async fn close_external(&self) -> Result<(), PgError> {
        let _access = self.instance.access_lock.lock().await;
        if self.instance.access_state.load(Ordering::Acquire) != CLOSED {
            // A pre-authentication backend can retain the HBA rules inherited at
            // fork and is not yet terminable via pg_stat_activity. Fast shutdown
            // drains every socket; the owned restart installs closed HBA rules
            // before accepting anything, including replication/admin reconnects.
            self.instance.restart_access_closed().await?;
        }
        Ok(())
    }

    pub async fn grant_role_access(&self) -> Result<(), PgError> {
        let _access = self.instance.access_lock.lock().await;
        if !self.instance.is_running().await {
            return Err(PgError::Process(
                "cannot grant access to a stopped PostgreSQL run".into(),
            ));
        }
        if self.instance.access_state.load(Ordering::Acquire) == GRANTED {
            return Ok(());
        }
        // A cancelled or failed grant still requires a full fence on the next
        // close, even if the reload acknowledgement has not completed.
        self.instance
            .access_state
            .store(GRANTING, Ordering::Release);
        let granted = self.grant_and_confirm().await;
        match granted {
            Ok(()) => {
                self.instance.access_state.store(GRANTED, Ordering::Release);
                Ok(())
            }
            Err(error) => Err(error.with_cleanup(self.instance.restart_access_closed().await)),
        }
    }

    async fn grant_and_confirm(&self) -> Result<(), PgError> {
        PgConfig::write_access_rules(
            self.instance.data_dir(),
            true,
            APPLICATION_ROLE,
            APPLICATION_DATABASE,
        )
        .await?;
        let (client, _connection) = self.instance.connect().await?;
        client
            .query_one("SELECT pg_reload_conf()", &[])
            .await
            .map_err(|error| PgError::Query(format!("reload granted access rules: {error}")))?;
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Ok((probe, connection)) = self.instance.connect_application().await {
                    drop(probe);
                    connection.await.map_err(|error| {
                        PgError::Connection(format!("join access probe: {error}"))
                    })?;
                    return Ok::<_, PgError>(());
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .map_err(|_| {
            PgError::Connection("application access reload was not acknowledged".into())
        })??;
        Ok(())
    }
}

#[cfg(all(test, feature = "testing"))]
mod tests {
    use super::*;
    use crate::testing::{ProcessProbe, TestDataDir, allocate_port, find_pg_bin};

    #[tokio::test]
    async fn cancelled_grant_remains_fence_required() {
        let directory = TestDataDir::new("cancel-grant");
        let instance = PgInstanceManager::new(
            directory.path().join("pgdata"),
            find_pg_bin(),
            allocate_port().await,
        );
        instance.init_db().await.unwrap();
        let (faults, _faults) = tokio::sync::mpsc::channel(8);
        instance.start_native(faults).await.unwrap();
        initialize_application_role(&instance).await.unwrap();
        let access = PgAccessController::new(&instance);
        for _ in 0..4 {
            let old = ProcessProbe::postgres(instance.data_dir());
            {
                let grant = access.grant_role_access();
                tokio::pin!(grant);
                tokio::select! {
                    biased;
                    result = &mut grant => panic!("grant completed before cancellation: {result:?}"),
                    _ = async {
                        while instance.access_state.load(Ordering::Acquire) != GRANTING {
                            tokio::task::yield_now().await;
                        }
                    } => {}
                }
            }
            assert_eq!(instance.access_state.load(Ordering::Acquire), GRANTING);
            access.close_external().await.unwrap();
            old.assert_reaped();
            assert_eq!(instance.access_state.load(Ordering::Acquire), CLOSED);
            assert!(instance.connect_application().await.is_err());
        }
        instance.stop().await.unwrap();
    }
}

pub async fn initialize_application_role(instance: &PgInstanceManager) -> Result<(), PgError> {
    let (client, _connection) = instance.connect().await?;
    client
        .batch_execute(&format!(
            "DO $$ BEGIN \
                 IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = '{role}') THEN \
                   CREATE ROLE {role} LOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION; \
                 END IF; \
                 IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'kuberic_rewind') THEN \
                   CREATE ROLE kuberic_rewind LOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION; \
                 END IF; \
               END $$; \
               GRANT pg_read_server_files TO kuberic_rewind; \
               GRANT EXECUTE ON FUNCTION pg_catalog.pg_read_binary_file(text) TO kuberic_rewind; \
               GRANT EXECUTE ON FUNCTION pg_catalog.pg_read_binary_file(text,bigint,bigint,boolean) TO kuberic_rewind; \
               GRANT EXECUTE ON FUNCTION pg_catalog.pg_stat_file(text,boolean) TO kuberic_rewind; \
               GRANT EXECUTE ON FUNCTION pg_catalog.pg_ls_dir(text,boolean,boolean) TO kuberic_rewind; \
               ALTER ROLE {role} SET synchronous_commit = 'remote_apply';",
            role = APPLICATION_ROLE,
        ))
        .await
        .map_err(|error| PgError::Query(format!("create application role: {error}")))?;
    let exists: bool = client
        .query_one(
            "SELECT EXISTS(SELECT 1 FROM pg_database WHERE datname = $1)",
            &[&APPLICATION_DATABASE],
        )
        .await
        .map_err(|error| PgError::Query(format!("query application database: {error}")))?
        .get(0);
    if !exists {
        client
            .batch_execute(&format!(
                "CREATE DATABASE {database} OWNER {role}",
                database = APPLICATION_DATABASE,
                role = APPLICATION_ROLE,
            ))
            .await
            .map_err(|error| PgError::Query(format!("create application database: {error}")))?;
    }
    Ok(())
}

pub async fn application_role_uses_remote_apply(
    instance: &PgInstanceManager,
) -> Result<bool, PgError> {
    let (client, _connection) = instance.connect().await?;
    let setting: Option<Vec<String>> = client
        .query_one(
            "SELECT rolconfig FROM pg_roles WHERE rolname = $1",
            &[&APPLICATION_ROLE],
        )
        .await
        .map_err(|error| PgError::Query(format!("query application role: {error}")))?
        .get(0);
    Ok(setting.is_some_and(|settings| {
        settings
            .iter()
            .any(|setting| setting == "synchronous_commit=remote_apply")
    }))
}

pub async fn stop_fence(instance: &PgInstanceManager) -> Result<(), PgError> {
    instance.stop().await?;
    if instance.is_running().await {
        return Err(PgError::Process(
            "PostgreSQL remained running after completed stop fence".into(),
        ));
    }
    Ok(())
}
