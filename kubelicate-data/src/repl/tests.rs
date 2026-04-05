use std::collections::HashMap;

use async_trait::async_trait;
use bytes::Bytes;
use futures::StreamExt;

use crate::{
    traits2::{BackgroundCopyStateProvider, BackgroundStateReplicator},
    types::{Lsn, TokioReceiverStream},
};

#[derive(Debug)]
pub struct KvStateProviderInner {
    commit_lsn: Lsn,
    inner: HashMap<String, String>,
}

#[derive(Debug)]
pub struct KvStateProvider {
    inner: std::sync::Mutex<KvStateProviderInner>,
}

#[async_trait]
impl crate::traits2::StateProvider for KvStateProvider {
    async fn update_epoch(
        &self,
        _epoch: crate::types::Epoch,
        _token: crate::types::CancelToken,
    ) -> crate::Result<()> {
        Ok(())
    }

    fn get_last_committed_sequence_number(&self) -> crate::Result<Lsn> {
        Ok(self.inner.lock().unwrap().commit_lsn)
    }

    async fn on_data_loss(
        &self,
        _token: crate::types::CancelToken,
    ) -> crate::Result<crate::types::DataLossAction> {
        Ok(crate::types::DataLossAction::None)
    }

    fn produce_copy_state(
        &mut self,
        _up_to_lsn: crate::types::Lsn,
        _copy_context_stream: crate::types::TokioReceiverStream<Bytes>,
    ) -> crate::Result<crate::types::TokioReceiverStream<Bytes>> {
        // for all kv pairs produce a copy operation.
        let (tx, rx) = tokio::sync::mpsc::channel(100);
        let inner = self.inner.lock().unwrap();
        let copy_map = inner.inner.clone();
        tokio::spawn(async move {
            for (k, v) in copy_map.iter() {
                let data = CopyData::KeyValue(k.clone(), v.clone());
                let data_str = serde_json::to_string(&data).unwrap();
                if let Err(e) = tx.send(Bytes::from(data_str)).await {
                    tracing::error!("Error sending copy data: {}", e);
                    break;
                }
            }
        });
        Ok(TokioReceiverStream::new(rx))
    }

    fn get_oldest_sequence_number(&self) -> crate::Result<Lsn> {
        Ok(0)
    }
}

impl KvStateProvider {
    pub async fn background_loop_primary_or_active_secondary(
        &self,
        mut b_state: Box<dyn BackgroundStateReplicator>,
        token: crate::types::CancelToken,
    ) {
        let (ack_sender, ack_receiver) = tokio::sync::mpsc::channel(100);

        // apply wal in background.
        let mut stream = b_state.replication_stream(TokioReceiverStream::new(ack_receiver));

        loop {
            let item = tokio::select! {
                item = stream.next() => {
                    item
                }
                _ = token.cancelled() => {
                    tracing::info!("Background loop primary cancelled");
                    break;
                }
            };
            let item = match item {
                Some(item) => item,
                None => {
                    tracing::info!("Replication stream ended");
                    break;
                }
            };
            // apply the item.
            let wal_data = serde_json::from_slice::<WalData>(&item.data).unwrap();
            let lsn = item.lsn;
            {
                let mut inner = self.inner.lock().unwrap();
                match wal_data {
                    WalData::Insert(k, v) => {
                        inner.inner.insert(k, v);
                    }
                    WalData::Delete(k) => {
                        inner.inner.remove(&k);
                    }
                }
                if lsn > inner.commit_lsn {
                    inner.commit_lsn = lsn;
                }
            }
            // ack the lsn.
            if let Err(e) = ack_sender.send(lsn).await {
                tracing::error!("Error sending ack: {}", e);
            }
        }
    }

    /// TODO: handle copy data.
    pub async fn background_loop_idle_secondary(
        &self,
        mut b_state: Box<dyn BackgroundCopyStateProvider>,
        token: crate::types::CancelToken,
    ) {
        let (ack_sender, ack_receiver) = tokio::sync::mpsc::channel(100);

        // apply wal in background.
        let mut stream = b_state.copy_state_stream(TokioReceiverStream::new(ack_receiver));

        loop {
            let item = tokio::select! {
                item = stream.next() => {
                    item
                }
                _ = token.cancelled() => {
                    tracing::info!("Background loop idle secondary cancelled");
                    break;
                }
            };
            let item = match item {
                Some(item) => item,
                None => {
                    tracing::info!("Copy state stream ended");
                    break;
                }
            };
            // apply the item.
            let wal_data = serde_json::from_slice::<WalData>(&item.data).unwrap();
            {
                let mut inner = self.inner.lock().unwrap();
                match wal_data {
                    WalData::Insert(k, v) => {
                        inner.inner.insert(k, v);
                    }
                    WalData::Delete(k) => {
                        inner.inner.remove(&k);
                    }
                }
            }
            // ack the lsn.
            if let Err(e) = ack_sender.send(item.lsn).await {
                tracing::error!("Error sending ack: {}", e);
            }
        }
    }
}

// Only one transaction at a time.

#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub enum WalData {
    Insert(String, String),
    Delete(String),
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub enum CopyData {
    KeyValue(String, String),
}

pub struct TransactionManagerInner {
    replicator: Box<dyn crate::traits2::StateReplicator>,
}

pub struct TransactionManager {
    inner: tokio::sync::Mutex<TransactionManagerInner>,
}

impl TransactionManager {
    pub async fn try_commit(
        &self,
        key: String,
        value: String,
        token: crate::types::CancelToken,
    ) -> crate::Result<Lsn> {
        let data = WalData::Insert(key, value);
        let data_str = serde_json::to_string(&data).unwrap();

        let mut inner = self.inner.lock().await;
        let lsn = inner
            .replicator
            .replicate(Bytes::from(data_str), token)
            .await?;
        // apply is done by background job.
        Ok(lsn)
    }
}

#[tokio::test]
async fn test_xedio_replicator() {

    // Your test code here
}
