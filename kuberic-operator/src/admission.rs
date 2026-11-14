use anyhow::{ensure, Result};
use tokio::sync::Mutex;

use crate::Allocation;

/// Backend-local admission retained across controller-facade recreation.
/// Reserve only for fresh, unprovisioned resources, never to import API status.
#[derive(Default)]
pub struct BackendAdmission {
    allocation: Mutex<Option<Allocation>>,
}

impl BackendAdmission {
    pub async fn reserve(&self, allocation: &Allocation) -> Result<()> {
        let mut original = self.allocation.lock().await;
        if let Some(original) = original.as_ref() {
            ensure!(
                original == allocation,
                "backend already admitted a different allocation"
            );
        } else {
            *original = Some(allocation.clone());
        }
        Ok(())
    }

    pub async fn allocation(&self) -> Option<Allocation> {
        self.allocation.lock().await.clone()
    }

    pub async fn require(&self, allocation: &Allocation) -> Result<()> {
        ensure!(
            self.allocation.lock().await.as_ref() == Some(allocation),
            "backend has no original allocation admission; status cannot reconstruct it"
        );
        Ok(())
    }
}
