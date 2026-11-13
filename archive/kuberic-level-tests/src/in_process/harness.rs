use anyhow::{Context, Result};
use kuberic_controller::test_util::{
    ApplicationFactory, CreatedApplication, ReplicaContext, ReplicaRef, ResourceKind as Kind,
    ResourceRef, TestCluster,
};
use kuberic_runtime::{
    host::ApplicationStorageState, protocol::public_operations::PublicV1AgentReport,
};
use kvstore2::{persistence::KvPersistence, service::KvService, state::CopyGate};
use std::{collections::BTreeMap, path::PathBuf, sync::Arc};

pub(super) struct KvFactory;
#[async_trait::async_trait]
impl ApplicationFactory for KvFactory {
    type Service = KvService;
    async fn create(
        &self,
        context: ReplicaContext,
    ) -> kuberic_controller::test_util::Result<CreatedApplication<KvService>> {
        let root = context.application_root().to_owned();
        let fresh = KvPersistence::is_fresh_empty(&root).map_err(anyhow::Error::from)?;
        let persistence = Arc::new(KvPersistence::open(&root).map_err(anyhow::Error::from)?);
        Ok(CreatedApplication::new(
            Arc::new(KvService::new(
                persistence,
                context.replication_address().into(),
                CopyGate::enabled(root.join("copy-gate"))?,
            )),
            if fresh {
                ApplicationStorageState::FreshEmpty
            } else {
                ApplicationStorageState::Established
            },
        )
        .with_storage_paths([root]))
    }
    async fn shutdown_and_drain(
        &self,
        service: Arc<KvService>,
    ) -> kuberic_controller::test_util::Result<()> {
        use kuberic_runtime::RuntimeError;
        if let Err(error) = service.shutdown_and_drain().await {
            if !matches!(error, RuntimeError::OperationCancelled) {
                return Err(anyhow::Error::from(error).into());
            }
            // Only typed retirement is expected; application faults remain fatal.
            if !matches!(
                service.get("__retired_probe").await,
                Err(RuntimeError::NotOpen)
            ) || !matches!(
                service
                    .replicate_put("__retired_probe".into(), "forbidden".into())
                    .await,
                Err(RuntimeError::NotOpen)
            ) {
                return Err(anyhow::anyhow!("aborted KV service retained client access").into());
            }
            eprintln!("verified retired KV service: {error}");
        }
        Ok(())
    }
}

#[derive(Default)]
pub(super) struct Snapshot {
    resources: Vec<(ResourceRef, serde_json::Value)>,
}
impl Snapshot {
    pub fn snapshot(&self, kind: Kind) -> Vec<serde_json::Value> {
        self.resources
            .iter()
            .filter(|(reference, _)| reference.kind() == kind)
            .map(|(_, value)| value.clone())
            .collect()
    }
    pub fn get(&self, kind: Kind, name: &str) -> Option<serde_json::Value> {
        self.resources
            .iter()
            .find(|(reference, _)| reference.kind() == kind && reference.name() == name)
            .map(|(_, value)| value.clone())
    }
    pub fn reference(&self, kind: Kind, uid: &str) -> Result<ResourceRef> {
        self.resources
            .iter()
            .find(|(reference, _)| reference.kind() == kind && reference.uid() == uid)
            .map(|(reference, _)| reference.clone())
            .context("resource reference missing")
    }
}

/// KV request/data assertions only; all orchestration is the controller's public harness.
pub(super) struct Harness {
    pub cluster: TestCluster<KvFactory>,
    pub api: Snapshot,
    refs: BTreeMap<String, ReplicaRef>,
    pub reconcile_errors: usize,
}
impl Harness {
    pub async fn new() -> Result<Self> {
        Self::with_members(3).await
    }
    pub async fn with_members(members: u32) -> Result<Self> {
        let cluster = TestCluster::builder(KvFactory)
            .image("in-process/kvstore2")
            .initial_replicas(members)
            .start()
            .await?;
        Ok(Self {
            cluster,
            api: Snapshot::default(),
            refs: BTreeMap::new(),
            reconcile_errors: 0,
        })
    }
    pub async fn copy_gate(&mut self, id: i64) -> Result<CopyGate> {
        let reference = self.reference(id).await?;
        Ok(CopyGate::enabled(
            self.cluster
                .application_root(&reference)
                .await?
                .join("copy-gate"),
        )?)
    }
    pub async fn refresh(&mut self) -> Result<()> {
        let observation = self.cluster.observe().await?;
        self.reconcile_errors = observation.reconcile_errors();
        self.api.resources = observation
            .resources()
            .iter()
            .map(|resource| (resource.reference().clone(), resource.value().clone()))
            .collect();
        for reference in observation.replicas() {
            self.refs
                .insert(reference.instance_id().into(), reference.clone());
        }
        Ok(())
    }
    pub async fn set(&mut self) -> Result<kuberic_controller::crd::KubericSet> {
        self.refresh().await?;
        serde_json::from_value(
            self.api
                .get(Kind::KubericSet, "sample")
                .context("set missing")?,
        )
        .map_err(Into::into)
    }
    pub async fn reference(&mut self, id: i64) -> Result<ReplicaRef> {
        self.refresh().await?;
        let topology = self
            .set()
            .await?
            .status
            .and_then(|status| status.public_v1_topology)
            .context("topology missing")?;
        let uid = topology
            .sessions
            .iter()
            .find(|session| session.identity.replica_id.value() == id)
            .context("member missing")?
            .identity
            .instance_id
            .to_string();
        self.reference_uid(&uid).await
    }
    pub async fn reference_uid(&mut self, uid: &str) -> Result<ReplicaRef> {
        self.refresh().await?;
        self.refs
            .get(uid)
            .cloned()
            .context("exact replica reference missing")
    }
    pub async fn ready(&mut self) -> Result<i64> {
        self.ready_members(3).await
    }
    pub async fn ready_members(&mut self, expected: usize) -> Result<i64> {
        let topology = self.cluster.settle_members(expected).await?;
        let primary = topology.primary().replica_id();
        self.refresh().await?;
        Ok(primary)
    }
    pub async fn ready_survivors(
        &mut self,
        expected: usize,
        isolation: &kuberic_controller::test_util::Isolation,
    ) -> Result<i64> {
        let topology = self.cluster.settle_survivors(isolation, expected).await?;
        let primary = topology.primary().replica_id();
        self.refresh().await?;
        Ok(primary)
    }
    pub async fn step(&mut self) -> Result<()> {
        self.cluster.step().await?;
        self.refresh().await
    }
    pub async fn service(&mut self, id: i64) -> Result<Arc<KvService>> {
        let reference = self.reference(id).await?;
        Ok(self.cluster.service(&reference).await?)
    }
    pub async fn service_uid(&mut self, uid: &str) -> Result<Arc<KvService>> {
        let reference = self.reference_uid(uid).await?;
        Ok(self.cluster.service(&reference).await?)
    }
    pub async fn report(&mut self, id: i64) -> Result<PublicV1AgentReport> {
        let reference = self.reference(id).await?;
        self.report_uid(reference.instance_id()).await
    }
    pub async fn report_uid(&mut self, uid: &str) -> Result<PublicV1AgentReport> {
        let reference = self.reference_uid(uid).await?;
        self.cluster
            .observe()
            .await?
            .report(&reference)
            .cloned()
            .context("agent report unavailable")
    }
    pub async fn live_uids(&mut self) -> Vec<String> {
        let _ = self.refresh().await;
        self.api
            .snapshot(Kind::Pod)
            .iter()
            .filter(|pod| {
                pod["metadata"]["annotations"][kuberic_controller::crd::CONTROL_ADDRESS_ANNOTATION]
                    .is_string()
            })
            .filter_map(|pod| pod["metadata"]["uid"].as_str().map(str::to_owned))
            .collect()
    }
    pub async fn data_root(&mut self, uid: &str) -> Result<PathBuf> {
        let reference = self.reference_uid(uid).await?;
        Ok(self
            .cluster
            .application_root(&reference)
            .await?
            .parent()
            .context("volume root missing")?
            .to_owned())
    }
    pub async fn stop(&mut self, id: i64, restart: bool) -> Result<()> {
        let reference = self.reference(id).await?;
        if restart {
            self.cluster.restart(&reference).await?;
        } else {
            self.cluster.stop(&reference).await?;
        }
        self.refresh().await
    }
    pub async fn recreate_controller(&mut self) -> Result<()> {
        self.cluster.recreate_controller().await?;
        self.refresh().await
    }
    pub async fn write(&mut self, key: &str, value: &str) -> Result<i64> {
        let primary = self.ready().await?;
        let service = self.service(primary).await?;
        Ok(self
            .cluster
            .probe(async move { service.replicate_put(key.into(), value.into()).await })
            .await?)
    }
    pub async fn assert_values(&mut self, key: &str, value: &str) -> Result<()> {
        self.assert_values_on_members(key, value, 3).await
    }
    pub async fn assert_values_on_members(
        &mut self,
        key: &str,
        value: &str,
        expected: usize,
    ) -> Result<()> {
        loop {
            let topology = self.cluster.settle_members(expected).await?;
            let mut matching = 0;
            for reference in topology.members() {
                let service = self.cluster.service(reference).await?;
                let found = if reference.identity() == topology.primary().identity() {
                    self.cluster.probe(service.get(key)).await?
                } else {
                    use kuberic_runtime::RuntimeError;
                    use kuberic_runtime::protocol::types::AccessStatus;
                    self.cluster
                        .probe(async {
                            anyhow::ensure!(matches!(
                                service.get(key).await,
                                Err(RuntimeError::ReadClosed(AccessStatus::NotPrimary))
                            ));
                            Ok::<_, anyhow::Error>(())
                        })
                        .await?;
                    use kuberic_runtime::engine::DurableState;
                    let committed = service
                        .persistence()
                        .durable_progress()
                        .await?
                        .committed_lsn;
                    service
                        .persistence()
                        .snapshot_at(committed)?
                        .get(key)
                        .cloned()
                };
                if found.as_deref() == Some(value) {
                    matching += 1;
                }
            }
            if matching == expected {
                return Ok(());
            }
            self.step().await?;
        }
    }
    pub async fn finish(mut self, result: Result<()>) -> Result<()> {
        if result.is_err()
            && let Ok(observation) = self.cluster.observe().await
        {
            eprintln!(
                "in-process pre-cleanup snapshot: {}",
                serde_json::json!({
                    "set": observation.set(),
                    "reconcileErrors": observation.reconcile_errors(),
                    "agents": observation.replicas()
                        .map(|reference| observation.report(reference))
                        .collect::<Vec<_>>(),
                })
            );
        }
        if let Err(error) = self.cluster.finish(result).await {
            if let Some(diagnostics) = error.diagnostics() {
                eprintln!("in-process failure diagnostics: {diagnostics}");
            }
            return Err(error.into());
        }
        Ok(())
    }
    pub async fn submit(
        &mut self,
        id: i64,
    ) -> Result<kuberic_controller::test_util::SwitchoverRequest> {
        let reference = self.reference(id).await?;
        Ok(self.cluster.submit_switchover(&reference).await?)
    }
    pub async fn isolate_uids(
        &mut self,
        uids: &[String],
    ) -> Result<kuberic_controller::test_util::Isolation> {
        let mut references = Vec::new();
        for uid in uids {
            references.push(self.reference_uid(uid).await?);
        }
        Ok(self.cluster.isolate(&references).await?)
    }
    pub async fn heal(
        &mut self,
        isolation: kuberic_controller::test_util::Isolation,
    ) -> Result<()> {
        self.cluster.heal(isolation).await?;
        Ok(())
    }
}
