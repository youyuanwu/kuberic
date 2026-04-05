use std::collections::HashMap;

use crate::{
    traits2::OperationSender,
    types::{Lsn, ReplicaId},
};

pub struct ReplMgr {
    progress: HashMap<ReplicaId, Lsn>,
    clients: HashMap<ReplicaId, Box<dyn OperationSender>>,
    // storage: Box<dyn crate::traits2::WalStorage>,
}

impl ReplMgr {
    // TODO: support quorum write
    pub async fn replicate(&mut self, lsn: Lsn, data: bytes::Bytes) -> crate::Result<Lsn> {
        // send to all clients.
        for (replica_id, client) in self.clients.iter_mut() {
            let repl_data = crate::types::ReplData {
                lsn,
                data: data.clone(),
            };
            client
                .send_replication_data_item(repl_data)
                .await
                .expect("error for repl not supported");
            self.progress.insert(*replica_id, lsn);
        }
        Ok(lsn)
    }
}
