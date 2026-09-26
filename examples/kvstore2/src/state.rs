use std::sync::Arc;

use async_trait::async_trait;
use futures::stream;
use kuberic_protocol::types::Epoch;
use kuberic_runtime::application::{OperationDataStream, StateProvider};
use kuberic_runtime::{Result, RuntimeError};

use crate::persistence::{KvPersistence, snapshot_stream};

pub struct KvStateProvider {
    persistence: Arc<KvPersistence>,
}

impl KvStateProvider {
    pub fn new(persistence: Arc<KvPersistence>) -> Self {
        Self { persistence }
    }

    pub fn persistence(&self) -> &Arc<KvPersistence> {
        &self.persistence
    }
}

#[async_trait]
impl StateProvider for KvStateProvider {
    async fn update_epoch(&self, epoch: Epoch, _previous_epoch_last_lsn: i64) -> Result<()> {
        self.persistence.update_epoch(epoch)
    }

    async fn last_committed_lsn(&self) -> Result<i64> {
        use kuberic_runtime::engine::DurableState;
        Ok(self.persistence.durable_progress().await?.committed_lsn)
    }

    async fn get_copy_context(&self) -> Result<OperationDataStream> {
        Ok(Box::pin(stream::empty()))
    }

    async fn get_copy_state(
        &self,
        up_to_lsn: i64,
        mut copy_context: OperationDataStream,
    ) -> Result<OperationDataStream> {
        use futures::StreamExt;
        if copy_context.next().await.is_some() {
            return Err(RuntimeError::Application(
                "kvstore2 does not use a copy context".into(),
            ));
        }
        snapshot_stream(self.persistence.snapshot_at(up_to_lsn)?)
    }

    async fn on_data_loss(&self) -> Result<bool> {
        Ok(false)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use bytes::Bytes;
    use futures::{StreamExt, stream};
    use kuberic_runtime::application::Operation;
    use kuberic_runtime::engine::DurableState;

    use super::*;

    async fn collect_copy(mut copy: OperationDataStream) -> Vec<Bytes> {
        let mut chunks = Vec::new();
        while let Some(chunk) = copy.next().await {
            chunks.push(chunk.unwrap());
        }
        chunks
    }

    #[tokio::test]
    async fn provider_freezes_copy_before_post_boundary_write_and_replays_identically() {
        let directory = tempfile::tempdir().unwrap();
        let persistence = Arc::new(KvPersistence::open(directory.path()).unwrap());
        persistence
            .apply(Operation {
                lsn: 1,
                committed_lsn: 0,
                data: KvPersistence::encode_put("key".into(), "old".into()).unwrap(),
            })
            .await
            .unwrap();
        persistence.commit(1).await.unwrap();
        let provider = KvStateProvider::new(persistence.clone());
        let frozen = provider
            .get_copy_state(1, Box::pin(stream::empty()))
            .await
            .unwrap();

        let post_boundary = Operation {
            lsn: 2,
            committed_lsn: 1,
            data: KvPersistence::encode_put("key".into(), "new".into()).unwrap(),
        };
        persistence.apply(post_boundary.clone()).await.unwrap();
        persistence.commit(2).await.unwrap();

        let first = collect_copy(frozen).await;
        let second = collect_copy(
            provider
                .get_copy_state(1, Box::pin(stream::empty()))
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(first, second);
        let snapshot: BTreeMap<String, String> = serde_json::from_slice(&first.concat()).unwrap();
        assert_eq!(snapshot.get("key").map(String::as_str), Some("old"));

        let retained = persistence
            .get_replication_operations(2, 2)
            .await
            .unwrap()
            .map(|operation| operation.unwrap())
            .collect::<Vec<_>>()
            .await;
        assert_eq!(retained, vec![post_boundary]);
    }

    #[tokio::test]
    async fn provider_empty_state_copy_uses_boundary_zero() {
        let directory = tempfile::tempdir().unwrap();
        let persistence = Arc::new(KvPersistence::open(directory.path()).unwrap());
        let provider = KvStateProvider::new(persistence);
        let chunks = collect_copy(
            provider
                .get_copy_state(0, Box::pin(stream::empty()))
                .await
                .unwrap(),
        )
        .await;
        let snapshot: BTreeMap<String, String> = serde_json::from_slice(&chunks.concat()).unwrap();
        assert!(snapshot.is_empty());
    }
}
