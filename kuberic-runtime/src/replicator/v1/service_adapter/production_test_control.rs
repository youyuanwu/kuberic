//! Actual production effects assembled with test-owned facade/transport owners.

use super::*;

pub(crate) async fn test_compose(
    context: ReplicatorFactoryContext,
    provider: Arc<dyn StateProvider>,
    registry: MessageRegistry,
    transport: Arc<Transport>,
) -> Result<ReplicatorInterfaces> {
    compose(context, provider, registry, transport).await
}

pub(crate) fn test_provider(
    provider: Arc<dyn StateProvider>,
    transport: Arc<Transport>,
) -> Arc<dyn StateProvider> {
    Arc::new(SessionProvider {
        provider,
        transport,
    })
}

pub(crate) fn test_control(
    context: ReplicatorFactoryContext,
    facade: Arc<Facade>,
    transport: Arc<Transport>,
) -> Arc<dyn Replicator> {
    Arc::new(Production {
        settings_address: transport.local().address.clone(),
        facade,
        transport,
        context: context.clone(),
        peers: Arc::new(Mutex::new(BTreeMap::new())),
        epoch: Mutex::new(None),
        close: Mutex::new(None),
        builds: Mutex::new(BTreeMap::new()),
    })
}
