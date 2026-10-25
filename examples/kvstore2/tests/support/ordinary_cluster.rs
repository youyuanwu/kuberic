use std::sync::Arc;
use std::time::Duration;

use kuberic_controller::test_util::{
    ApplicationFactory, CreatedApplication, ReplicaContext, ReplicaRef, TestCluster,
};
use kuberic_runtime::RuntimeError;
use kuberic_runtime::host::ApplicationStorageState;
use kuberic_runtime::protocol::public_operations::{
    PublicV1CallKind, ReplicaAgentSnapshot, ReplicaGoal, ReplicaGoalStatus,
};
use kvstore2::{persistence::KvPersistence, service::KvService, state::CopyGate};

pub struct KvFactory;

#[async_trait::async_trait]
impl ApplicationFactory for KvFactory {
    type Service = KvService;

    async fn create(
        &self,
        context: ReplicaContext,
    ) -> kuberic_controller::test_util::Result<CreatedApplication<KvService>> {
        let root = context.application_root().to_owned();
        let error = |error| {
            kuberic_controller::test_util::TestError::with_source(
                kuberic_controller::test_util::ErrorKind::Application,
                "KV provider initialization failed",
                error,
            )
        };
        let fresh = KvPersistence::is_fresh_empty(&root).map_err(error)?;
        Ok(CreatedApplication::new(
            Arc::new(KvService::new(
                Arc::new(KvPersistence::open(&root).map_err(error)?),
                context.replication_address().into(),
                CopyGate::disabled(),
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
        match service.shutdown_and_drain().await {
            Ok(()) => Ok(()),
            Err(RuntimeError::OperationCancelled) => {
                assert!(service.get("retired-probe").await.is_err());
                assert!(
                    service
                        .replicate_put("retired-probe".into(), "forbidden".into())
                        .await
                        .is_err()
                );
                Ok(())
            }
            Err(error) => Err(kuberic_controller::test_util::TestError::with_source(
                kuberic_controller::test_util::ErrorKind::Cleanup,
                "KV application drain failed",
                error,
            )),
        }
    }
}

pub struct Replica {
    pub reference: ReplicaRef,
    pub service: Arc<KvService>,
}

pub struct Fixture {
    pub cluster: TestCluster<KvFactory>,
    pub replicas: Vec<Replica>,
}

impl Fixture {
    pub async fn start(name: &str, members: u32) -> Self {
        let mut cluster = TestCluster::builder(KvFactory)
            .name(name)
            .initial_replicas(members)
            .scenario_timeout(Duration::from_secs(30))
            .start()
            .await
            .unwrap();
        let topology = cluster.settle_members(members as usize).await.unwrap();
        let references = std::iter::once(topology.primary().clone())
            .chain(topology.secondaries().cloned())
            .collect::<Vec<_>>();
        let mut replicas = Vec::new();
        for reference in references {
            let service = cluster.service(&reference).await.unwrap();
            replicas.push(Replica { reference, service });
        }
        Self { cluster, replicas }
    }

    pub async fn snapshot(&mut self, index: usize) -> ReplicaAgentSnapshot {
        self.cluster
            .observe()
            .await
            .unwrap()
            .snapshot(&self.replicas[index].reference)
            .unwrap()
            .clone()
    }

    pub async fn terminal(&mut self, index: usize, kind: PublicV1CallKind) -> ReplicaGoal {
        let goal = self
            .cluster
            .retain_ordinary_terminal(&self.replicas[index].reference, kind)
            .await
            .unwrap();
        loop {
            self.cluster.step().await.unwrap();
            if self.snapshot(index).await.withdrawn {
                return goal;
            }
        }
    }

    pub async fn settled(&mut self, index: usize, goal: &ReplicaGoal) -> ReplicaGoalStatus {
        loop {
            self.cluster.step().await.unwrap();
            let snapshot = self.snapshot(index).await;
            if let Some(entry) = snapshot.goals.iter().find(|entry| &entry.goal == goal)
                && entry.status != ReplicaGoalStatus::Pending
            {
                return entry.status.clone();
            }
        }
    }

    pub async fn finish(self, result: kuberic_controller::test_util::Result<()>) {
        if let Err(error) = self.cluster.finish(result).await {
            eprintln!("{error}");
            for source in error.sources() {
                let mut current: Option<&(dyn std::error::Error + 'static)> = Some(source);
                while let Some(error) = current {
                    eprintln!("caused by: {error}");
                    current = error.source();
                }
            }
            if let Some(diagnostics) = error.diagnostics() {
                eprintln!("{}", serde_json::to_string_pretty(diagnostics).unwrap());
            }
            panic!("ordinary KV scenario failed: {error}");
        }
    }
}
