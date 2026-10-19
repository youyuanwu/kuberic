use std::sync::Arc;

use async_trait::async_trait;
use kuberic_controller::test_util::{
    ApplicationFactory, CreatedApplication, ReplicaContext, Result,
};
use kuberic_runtime::host::ApplicationStorageState;

use super::{CounterService, CounterStore};

pub struct CounterFactory;

#[async_trait]
impl ApplicationFactory for CounterFactory {
    type Service = CounterService;

    async fn create(&self, context: ReplicaContext) -> Result<CreatedApplication<Self::Service>> {
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
        ))
    }

    async fn shutdown_and_drain(&self, service: Arc<Self::Service>) -> Result<()> {
        service
            .shutdown_and_drain()
            .await
            .map_err(anyhow::Error::new)?;
        Ok(())
    }
}
