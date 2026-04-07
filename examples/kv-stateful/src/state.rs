use std::collections::HashMap;
use std::sync::Arc;

use kubelicate_core::types::{CancellationToken, Lsn, OperationStream};
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use tracing::{info, warn};

#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum KvOp {
    Put { key: String, value: String },
    Delete { key: String },
}

pub struct KvState {
    pub data: HashMap<String, String>,
    pub last_applied_lsn: Lsn,
}

impl Default for KvState {
    fn default() -> Self {
        Self::new()
    }
}

impl KvState {
    pub fn new() -> Self {
        Self {
            data: HashMap::new(),
            last_applied_lsn: 0,
        }
    }

    pub fn apply_op(&mut self, lsn: Lsn, op: &KvOp) {
        match op {
            KvOp::Put { key, value } => {
                self.data.insert(key.clone(), value.clone());
            }
            KvOp::Delete { key } => {
                self.data.remove(key);
            }
        }
        if lsn > self.last_applied_lsn {
            self.last_applied_lsn = lsn;
        }
    }
}

pub type SharedState = Arc<RwLock<KvState>>;

/// Drain a copy or replication stream, applying each operation to shared state.
/// Stops when the stream ends or the cancellation token fires.
pub async fn drain_stream(
    state: SharedState,
    mut stream: OperationStream,
    token: CancellationToken,
    label: &'static str,
) {
    loop {
        tokio::select! {
            biased;
            _ = token.cancelled() => {
                info!(label, "stream drain cancelled");
                break;
            }
            item = stream.get_operation() => {
                let Some(op) = item else { break };
                let lsn = op.lsn;
                match serde_json::from_slice::<KvOp>(&op.data) {
                    Ok(kv_op) => {
                        state.write().await.apply_op(lsn, &kv_op);
                        info!(lsn, ?kv_op, label, "applied from stream");
                    }
                    Err(e) => {
                        warn!(lsn, error = %e, label, "failed to deserialize stream op");
                    }
                }
                op.acknowledge();
            }
        }
    }
    info!(label, "stream drained");
}
