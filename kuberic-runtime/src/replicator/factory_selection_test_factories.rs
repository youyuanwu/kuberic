// Included only inside the cfg(test) SDK selection module. Keep deliberate
// independent/delegating constructors out of the production ownership source.
struct IndependentFactory {
    calls: Arc<AtomicUsize>,
    requirements: StdMutex<Vec<bool>>,
    control: Arc<IndependentControl>,
}

fn independent_factory(calls: Arc<AtomicUsize>) -> IndependentFactory {
    IndependentFactory {
        calls,
        requirements: StdMutex::new(Vec::new()),
        control: Arc::new(IndependentControl::default()),
    }
}

#[async_trait]
impl ReplicatorFactory for IndependentFactory {
    async fn create_replicator(
        &self,
        context: ReplicatorFactoryContext,
        _: Option<Arc<dyn StateProvider>>,
        _: ReplicatorSettings,
    ) -> Result<ReplicatorInterfaces> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.requirements
            .lock()
            .unwrap()
            .push(context.builtin_history_bound());
        Ok(ReplicatorInterfaces::secondary(
            self.control.clone(),
            None,
            ReplicatorCapabilities::conservative(),
        ))
    }
}

struct DefaultWrapper {
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl ReplicatorFactory for DefaultWrapper {
    async fn create_replicator(
        &self,
        context: ReplicatorFactoryContext,
        provider: Option<Arc<dyn StateProvider>>,
        settings: ReplicatorSettings,
    ) -> Result<ReplicatorInterfaces> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        DefaultReplicatorFactory::new()
            .create_replicator(context, provider, settings)
            .await
    }
}

fn default_factory() -> DefaultReplicatorFactory {
    PUBLIC_V1_FACTORIES.construct_default()
}
