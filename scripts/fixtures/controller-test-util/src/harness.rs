use std::sync::Arc;

use async_trait::async_trait;
use kuberic_controller::test_util::{
    ApplicationFactory, CreatedApplication, ReplicaContext, Result,
};
use kuberic_runtime::host::ApplicationStorageState;

use super::{CounterError, CounterRequest, CounterService, CounterStore};

pub struct CounterFactory;

#[async_trait]
impl ApplicationFactory for CounterFactory {
    type Service = CounterService;

    async fn create(&self, context: ReplicaContext) -> Result<CreatedApplication<Self::Service>> {
        if context.is_process_proxy() {
            return Ok(CreatedApplication::new(
                Arc::new(CounterService::process_proxy(context)),
                // Unused by the process backend; only the child classifies storage.
                ApplicationStorageState::Established,
            ));
        }
        let (store, fresh) =
            CounterStore::open(context.application_root()).map_err(anyhow::Error::new)?;
        Ok(CreatedApplication::new(
            Arc::new(CounterService::new(
                store,
                context.replication_address().to_owned(),
            )),
            if fresh {
                ApplicationStorageState::FreshEmpty
            } else {
                ApplicationStorageState::Established
            },
        )
        .with_storage_paths([context.application_root().to_owned()]))
    }

    async fn shutdown_and_drain(&self, service: Arc<Self::Service>) -> Result<()> {
        service
            .shutdown_and_drain()
            .await
            .map_err(anyhow::Error::new)?;
        Ok(())
    }

    fn member_executable(&self) -> Option<std::path::PathBuf> {
        if let Some(executable) = std::env::var_os("KUBERIC_COUNTER_MEMBER_BIN") {
            return Some(executable.into());
        }
        if cfg!(feature = "process-members") {
            let current = std::env::current_exe().expect("current counter test executable");
            let directory = current.parent().expect("counter test executable directory");
            let directory = if directory.file_name().is_some_and(|name| name == "deps") {
                directory.parent().expect("Cargo binary directory")
            } else {
                directory
            };
            return Some(directory.join("kuberic-counter-member"));
        }
        None
    }

    fn recoverable_member_exit(
        &self,
        context: &ReplicaContext,
        pid: u32,
        status: std::process::ExitStatus,
    ) -> Result<bool> {
        if status.success() {
            return Ok(false);
        }
        context.validate_member_recovery_exit_cause(pid)?;
        Ok(true)
    }

    async fn member_data(
        &self,
        service: Arc<Self::Service>,
        request: serde_json::Value,
    ) -> Result<serde_json::Value> {
        let request: CounterRequest =
            serde_json::from_value(request).map_err(anyhow::Error::new)?;
        let result = match request {
            CounterRequest::Set(value) => service.set(value).await,
            CounterRequest::Read => service.read().await,
        }
        .map_err(CounterError::from);
        Ok(serde_json::to_value(result).map_err(anyhow::Error::new)?)
    }

    fn member_observation(&self, service: &Self::Service) -> Result<serde_json::Value> {
        let state = service.local_store().state.lock().unwrap();
        Ok(serde_json::json!({
            "openModes":service.try_open_modes().map_err(anyhow::Error::new)?,
            "committedValue":state.value_at(state.committed).map_err(anyhow::Error::new)?,
            "appliedLsn":state.applied,
            "committedLsn":state.committed,
        }))
    }
}

#[cfg(feature = "process-members")]
impl super::retained_witness::WitnessApplication for CounterFactory {
    fn write_request(value: i64) -> serde_json::Value {
        serde_json::json!({"Set": value})
    }
    fn read_request() -> serde_json::Value {
        serde_json::json!("Read")
    }
    fn write_lsn(response: serde_json::Value) -> anyhow::Result<i64> {
        response["Ok"]
            .as_i64()
            .ok_or_else(|| anyhow::anyhow!("counter write failed: {response}"))
    }
    fn read_value(response: serde_json::Value) -> anyhow::Result<i64> {
        response["Ok"]
            .as_i64()
            .ok_or_else(|| anyhow::anyhow!("counter read failed: {response}"))
    }
}
