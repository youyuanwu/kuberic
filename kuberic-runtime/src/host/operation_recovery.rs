//! Caller-independent recovery for dormant public-operation records.

use std::sync::Arc;

use tokio::sync::watch;

use crate::host::Result;
use crate::host::operation::PartitionOperationRegistry;

pub(crate) struct PartitionOperationRecoveryOwner {
    registry: Arc<PartitionOperationRegistry>,
}

impl PartitionOperationRecoveryOwner {
    pub(crate) fn new(registry: Arc<PartitionOperationRegistry>) -> Self {
        Self { registry }
    }

    pub(crate) async fn run(&self, mut shutdown: watch::Receiver<bool>) -> Result<()> {
        let mut revision = self.registry.revision_receiver();
        self.registry.recover_unowned().await?;
        loop {
            tokio::select! {
                changed = revision.changed() => {
                    if changed.is_err() {
                        return Ok(());
                    }
                    self.registry.recover_unowned().await?;
                }
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() {
                        return Ok(());
                    }
                }
            }
        }
    }
}
