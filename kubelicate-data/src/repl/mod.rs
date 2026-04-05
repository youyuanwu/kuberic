#![allow(dead_code, unused_variables)]
use std::collections::HashMap;

use async_trait::async_trait;
use tokio::sync::{Mutex, mpsc::Receiver};
use tokio_stream::StreamExt;
use tracing::{error, info};

use crate::{
    traits2::{Replicator, StateProvider},
    types::{Lsn, ReplicaId, ReplicaSetConfig},
};

#[cfg(test)]
mod tests;

mod mgr;
pub use mgr::ReplMgr;

pub struct XedioReplicator {}

pub enum PrimaryEvent {
    DemoteToSecondary {
        complete: tokio::sync::oneshot::Sender<crate::Result<()>>,
    },
    BuildReplica {
        info: crate::types::ReplicaInfo,
        complete: tokio::sync::oneshot::Sender<crate::Result<()>>,
    },
    CatchUpQuorum {
        replica_set: ReplicaSetConfig,
        complete: tokio::sync::oneshot::Sender<crate::Result<()>>,
    }, // TODO: remove replica
}

#[allow(dead_code)]
pub struct XedioReplicatorInternal {
    state: Mutex<Box<dyn StateProvider>>,
    replica_manager: ReplicaManager,
    // id of the current replica.
    id: ReplicaId,
    built_replicas: HashMap<ReplicaId, Lsn>,
}

impl std::fmt::Debug for XedioReplicatorInternal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("XedioReplicatorInternal")
            .field("state", &"Box<dyn StateProvider>")
            .field("replica_manager", &self.replica_manager)
            .finish()
    }
}

impl XedioReplicatorInternal {
    pub fn new(state: Box<dyn StateProvider>, id: ReplicaId) -> Self {
        Self {
            state: Mutex::new(state),
            replica_manager: ReplicaManager::new(),
            id,
            built_replicas: HashMap::new(),
        }
    }
}

impl XedioReplicatorInternal {
    pub async fn run_loop_primary(
        mut self,
        mut events: Receiver<PrimaryEvent>,
    ) -> crate::Result<Self> {
        while let Some(event) = events.recv().await {
            match event {
                PrimaryEvent::DemoteToSecondary { complete } => {
                    info!("Demoting to secondary");
                    let _ = complete.send(Ok(()));
                    break;
                }
                PrimaryEvent::BuildReplica { mut info, complete } => {
                    info!("Building replica {:?}", info.id);
                    let up_to_lsn = self.get_current_progress().await?;
                    let result = Self::build_replica_inner(up_to_lsn, &mut self.state, &mut info)
                        .await
                        .inspect_err(|e| {
                            error!("Failed to build replica: {:?}", e);
                        })
                        .inspect(|_| {
                            // save the progress.
                            self.built_replicas.insert(info.id, up_to_lsn);
                        });
                    let _ = complete.send(result);
                }
                PrimaryEvent::CatchUpQuorum {
                    mut replica_set,
                    complete,
                } => {
                    info!("Catching up quorum for replica set {:?}", replica_set);
                    // Assume no write is happening, so replicate all data to secondaries.
                    let current_lsn = self.get_current_progress().await?;
                    Self::catch_up_inner(current_lsn, &mut replica_set, complete).await;
                }
            }
        }
        Ok(self)
    }

    // TODO: run in background task.
    pub async fn build_replica_inner(
        up_to_lsn: crate::types::Lsn,
        state: &mut Mutex<Box<dyn StateProvider>>,
        replica: &mut crate::types::ReplicaInfo,
    ) -> crate::Result<()> {
        // Get the copy context stream from idle secondary.
        let copy_context_stream = replica.client.request_copy_context_stream().await?;
        // Data that idle secondary needs.
        let mut copy_state_stream = state
            .lock()
            .await
            .produce_copy_state(up_to_lsn, copy_context_stream)?;
        // Send the copy state stream to secondary.
        while let Some(item) = copy_state_stream.next().await {
            replica.client.send_copy_data_item(item).await?;
        }
        Ok(())
    }

    pub async fn catch_up_inner(
        current_lsn: crate::types::Lsn,
        replica_set: &mut ReplicaSetConfig,
        complete: tokio::sync::oneshot::Sender<crate::Result<()>>,
    ) {
        let _ = complete.send(Ok(()));
    }
}

#[async_trait]
impl Replicator for XedioReplicatorInternal {
    async fn open(&self, _token: crate::types::CancelToken) -> crate::Result<()> {
        Ok(())
    }

    async fn close(&self, _token: crate::types::CancelToken) -> crate::Result<()> {
        Ok(())
    }

    async fn change_role(
        &self,
        role: crate::types::Role,
        token: crate::types::CancelToken,
    ) -> crate::Result<()> {
        // match role{

        // }
        Ok(())
    }

    async fn update_epoch(
        &self,
        epoch: crate::types::Epoch,
        token: crate::types::CancelToken,
    ) -> crate::Result<()> {
        self.state.lock().await.update_epoch(epoch, token).await
        // TODO: other stuff
    }

    async fn get_address(&self, _token: crate::types::CancelToken) -> crate::Result<String> {
        todo!()
    }

    /// tail of log.
    async fn get_current_progress(&self) -> crate::Result<crate::types::Lsn> {
        self.state.lock().await.get_last_committed_sequence_number()
    }

    /// head of log.
    async fn get_catch_up_capability(&self) -> crate::Result<crate::types::Lsn> {
        todo!()
    }

    async fn on_data_loss(
        &self,
        token: crate::types::CancelToken,
    ) -> crate::Result<crate::types::DataLossAction> {
        todo!()
    }

    async fn update_catch_up_replica_set_configuration(
        &mut self,
        currentconfiguration: crate::types::ReplicaSetConfig,
        previousconfiguration: crate::types::ReplicaSetConfig,
    ) -> crate::Result<()> {
        self.replica_manager
            .update_configuration(currentconfiguration, previousconfiguration);
        Ok(())
    }

    async fn update_current_replica_set_configuration(
        &mut self,
        currentconfiguration: crate::types::ReplicaSetConfig,
    ) -> crate::Result<()> {
        self.replica_manager
            .update_current_configuration(currentconfiguration);
        Ok(())
    }

    async fn wait_for_catch_up_quorum(
        &self,
        mode: crate::types::ReplicaSetQuorumMode,
        token: crate::types::CancelToken,
    ) -> crate::Result<()> {
        todo!()
    }

    async fn build_replica(
        &self,
        replica: crate::types::ReplicaInfo,
        token: crate::types::CancelToken,
    ) -> crate::Result<()> {
        // TODO: track this replica.
        Ok(())
    }

    async fn remove_replica(&self, replicaid: crate::types::ReplicaId) -> crate::Result<()> {
        todo!()
    }
}

// #[async_trait]
// impl StateReplicator for XedioReplicatorInternal {
//     async fn replicate(
//         &self,
//         data: &bytes::Bytes,
//         token: crate::types::CancelToken,
//         lsn: &mut crate::types::Lsn,
//     ) -> crate::Result<()> {
//         todo!()
//     }

//     fn get_replication_stream(&self) -> TokioReceiverStream<crate::traits::ReplData> {
//         todo!()
//     }

//     fn ack_replication_data(&self, lsn: crate::types::Lsn) -> crate::Result<()> {
//         todo!()
//     }

//     fn get_copy_stream(&mut self) -> TokioReceiverStream<crate::traits::ReplData> {
//         self.copy_state_stream
//             .take()
//             .expect("Copy state stream is not set")
//     }

//     fn ack_copy_data(&self, lsn: crate::types::Lsn) -> crate::Result<()> {
//         self.copy_state_ack_tx
//             .as_ref()
//             .expect("Copy state ack tx is not set")
//             .try_send(lsn)
//             .map_err(|e| crate::Error::from(e))?;
//         Ok(())
//     }
// }

#[derive(Debug)]
pub struct ReplicaManager {
    replicas: HashMap<ReplicaId, Box<dyn crate::traits2::OperationSender>>,
    current: Vec<ReplicaId>,
    current_quorum: usize,
    previous: Vec<ReplicaId>,
    previous_quorum: usize,
}

impl Default for ReplicaManager {
    fn default() -> Self {
        Self::new()
    }
}

impl ReplicaManager {
    pub fn new() -> Self {
        Self {
            replicas: HashMap::new(),
            current: Vec::new(),
            current_quorum: 0,
            previous: Vec::new(),
            previous_quorum: 0,
        }
    }

    pub fn update_configuration(
        &mut self,
        current: crate::types::ReplicaSetConfig,
        previous: crate::types::ReplicaSetConfig,
    ) {
        let mut new_replicas = HashMap::new();
        for r in &current.members {
            new_replicas.insert(r.id, r.client.clone_box());
        }

        for r in &previous.members {
            new_replicas
                .entry(r.id)
                .or_insert_with(|| r.client.clone_box());
        }

        self.replicas = new_replicas;
        self.current = current.members.iter().map(|r| r.id).collect();
        self.current_quorum = current.write_quorum;
        self.previous = previous.members.iter().map(|r| r.id).collect();
        self.previous_quorum = previous.write_quorum;
    }

    pub fn update_current_configuration(&mut self, current: crate::types::ReplicaSetConfig) {
        let mut new_replicas = HashMap::new();
        for r in &current.members {
            new_replicas.insert(r.id, r.client.clone_box());
        }

        self.replicas = new_replicas;
        self.current = current.members.iter().map(|r| r.id).collect();
        self.current_quorum = current.write_quorum;

        self.previous.clear();
        self.previous_quorum = 0;
    }
}
