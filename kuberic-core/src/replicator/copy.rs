//! Copy protocol for BuildReplica — runs as a spawned async task within
//! the replicator. The actor's BuildReplica handler spawns this task
//! with a clone of `state_provider_tx` and `state`.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{mpsc, oneshot};
use tracing::info;

use crate::error::{KubericError, Result};
use crate::events::StateProviderEvent;
use crate::handles::PartitionState;
use crate::types::{CancellationToken, Lsn, OperationStream, ReplicaInfo};

/// Copy phase of BuildReplica — runs as a spawned async task.
/// Uses state_provider_tx to reach the user's state provider callbacks.
pub(crate) async fn run_build_replica_copy(
    replica: ReplicaInfo,
    build_key: Option<String>,
    state_provider_tx: mpsc::UnboundedSender<StateProviderEvent>,
    state: Arc<PartitionState>,
    reply_timeout: Duration,
    cancellation: CancellationToken,
) -> Result<Lsn> {
    use crate::proto::replicator_data_client::ReplicatorDataClient;

    let secondary_addr = &replica.replicator_address;
    info!(
        secondary_id = replica.id,
        %secondary_addr,
        "BuildReplica: connecting to secondary data plane"
    );

    let endpoint = tonic::transport::Channel::from_shared(secondary_addr.clone())
        .map_err(|e| KubericError::Internal(Box::new(e)))?;
    let channel = endpoint.connect();
    let channel = tokio::select! {
        biased;
        _ = cancellation.cancelled() => return Err(KubericError::Cancelled),
        result = channel => result,
    }
    .map_err(|e| KubericError::Internal(Box::new(e)))?;
    let mut data_client = ReplicatorDataClient::new(channel);

    let ctx_resp = tokio::select! {
        biased;
        _ = cancellation.cancelled() => return Err(KubericError::Cancelled),
        result = data_client.get_copy_context(crate::proto::GetCopyContextRequest {}) => result,
    }
    .map_err(|e| KubericError::Internal(Box::new(e)))?;
    let copy_context_ops = ctx_resp.into_inner().operations;
    info!(
        context_items = copy_context_ops.len(),
        "BuildReplica: got copy context from secondary"
    );

    let copy_context = vec_to_stream(
        copy_context_ops
            .into_iter()
            .map(|op| (op.lsn, bytes::Bytes::from(op.data)))
            .collect(),
    );

    let up_to_lsn = state.committed_lsn();
    let state_stream: OperationStream = {
        let (tx, rx) = oneshot::channel();
        state_provider_tx
            .send(StateProviderEvent::GetCopyState {
                up_to_lsn,
                copy_context,
                reply: tx,
            })
            .map_err(|_| KubericError::Closed)?;
        let result = tokio::select! {
            biased;
            _ = cancellation.cancelled() => return Err(KubericError::Cancelled),
            result = tokio::time::timeout(reply_timeout, rx) => result,
        };
        match result {
            Ok(Ok(result)) => result?,
            Ok(Err(_)) => return Err(KubericError::Closed),
            Err(_) => return Err(KubericError::Internal("state_provider timeout".into())),
        }
    };

    let state_ops = collect_stream(state_stream, cancellation.clone()).await?;

    // The state provider snapshot is authoritative through up_to_lsn even
    // when it emits no operations because the secondary already has the
    // requested state.
    let copy_lsn = up_to_lsn;
    info!(
        items = state_ops.len(),
        up_to_lsn, copy_lsn, "BuildReplica: got copy state from local StateProvider"
    );

    let mut items: Vec<crate::proto::CopyItem> = state_ops
        .into_iter()
        .map(|(lsn, data)| crate::proto::CopyItem {
            lsn,
            data: data.to_vec(),
            is_boundary: false,
        })
        .collect();
    items.push(crate::proto::CopyItem {
        lsn: copy_lsn,
        data: Vec::new(),
        is_boundary: true,
    });

    let item_stream = tokio_stream::iter(items);
    let resp = tokio::select! {
        biased;
        _ = cancellation.cancelled() => return Err(KubericError::Cancelled),
        result = data_client.copy_stream(item_stream) => result,
    }
    .map_err(|e| KubericError::Internal(Box::new(e)))?;

    info!(
        items_received = resp.into_inner().items_received,
        "BuildReplica: copy complete"
    );

    state.set_copy_lsn(replica.id, replica.instance_id, build_key, copy_lsn);
    Ok(copy_lsn)
}

/// Collect all operations from a stream into a Vec.
async fn collect_stream(
    mut stream: OperationStream,
    cancellation: CancellationToken,
) -> Result<Vec<(Lsn, bytes::Bytes)>> {
    let mut ops = Vec::new();
    loop {
        let operation = tokio::select! {
            biased;
            _ = cancellation.cancelled() => return Err(KubericError::Cancelled),
            operation = stream.get_operation() => operation,
        };
        let Some(op) = operation else {
            break;
        };
        ops.push((op.lsn, op.data.clone()));
        op.acknowledge();
    }
    Ok(ops)
}

/// Create an OperationStream from materialized operations.
fn vec_to_stream(ops: Vec<(Lsn, bytes::Bytes)>) -> OperationStream {
    let (tx, stream) = OperationStream::channel(ops.len().max(1));
    tokio::spawn(async move {
        for (lsn, data) in ops {
            let op = crate::types::Operation::new(lsn, data, None);
            if tx.send(op).await.is_err() {
                break;
            }
        }
    });
    stream
}
