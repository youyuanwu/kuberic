//! Caller-independent recovery for dormant public-operation records.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::watch;

use crate::host::Result;
use crate::host::operation::PartitionOperationRegistry;

pub(crate) struct PartitionOperationRecoveryOwner {
    registry: Arc<PartitionOperationRegistry>,
}

pub(crate) struct PartitionOperationRuntime {
    registry: Arc<PartitionOperationRegistry>,
    shutdown: watch::Sender<bool>,
    task: Option<tokio::task::JoinHandle<Result<()>>>,
}

impl PartitionOperationRuntime {
    pub(crate) fn start(registry: Arc<PartitionOperationRegistry>) -> Self {
        let owner = PartitionOperationRecoveryOwner::new(registry.clone());
        let (shutdown, receiver) = watch::channel(false);
        let task = tokio::spawn(async move { owner.run(receiver).await });
        Self {
            registry,
            shutdown,
            task: Some(task),
        }
    }

    pub(crate) fn registry(&self) -> Arc<PartitionOperationRegistry> {
        self.registry.clone()
    }

    pub(crate) async fn shutdown(mut self) -> Result<()> {
        self.shutdown.send_replace(true);
        self.task
            .take()
            .expect("public-operation recovery owner task exists")
            .await
            .map_err(|error| {
                crate::host::HostError::CommandRejected(format!(
                    "public-operation recovery owner join failed: {error}"
                ))
            })?
    }
}

impl Drop for PartitionOperationRuntime {
    fn drop(&mut self) {
        self.shutdown.send_replace(true);
    }
}

impl PartitionOperationRecoveryOwner {
    pub(crate) fn new(registry: Arc<PartitionOperationRegistry>) -> Self {
        Self { registry }
    }

    pub(crate) async fn run(&self, mut shutdown: watch::Receiver<bool>) -> Result<()> {
        let mut revision = self.registry.revision_receiver();
        loop {
            if let Err(error) = self.registry.recover_unowned().await {
                tracing::warn!(%error, "public-operation recovery scan failed");
            }
            tokio::select! {
                changed = revision.changed() => {
                    if changed.is_err() {
                        break;
                    }
                }
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() {
                        break;
                    }
                }
                _ = tokio::time::sleep(Duration::from_millis(50)) => {}
            }
        }
        self.registry.shutdown().await
    }
}
