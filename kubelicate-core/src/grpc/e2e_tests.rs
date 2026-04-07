#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use tonic::transport::Server;

    use crate::driver::PartitionDriver;
    use crate::events::ReplicatorChannels;
    use crate::grpc::handle::GrpcReplicaHandle;
    use crate::grpc::server::ControlServer;
    use crate::handles::PartitionState;
    use crate::proto::replicator_control_server::ReplicatorControlServer;
    use crate::proto::replicator_data_server::ReplicatorDataServer;
    use crate::replicator::actor::WalReplicatorActor;
    use crate::replicator::secondary::{SecondaryReceiver, SecondaryState};
    use crate::types::{CancellationToken, ReplicaId};

    /// Spawn a full replica pod: replicator actor + data gRPC server + control gRPC server.
    /// Returns (control_address, data_address, shutdown_token).
    async fn spawn_pod(id: ReplicaId) -> (String, String, CancellationToken) {
        let channels = ReplicatorChannels::new(16, 256);
        let state = Arc::new(PartitionState::new());
        let secondary_state = Arc::new(SecondaryState::new());
        let shutdown = CancellationToken::new();

        // Data plane gRPC server (secondary receiver)
        let data_receiver = SecondaryReceiver::new(secondary_state);
        let data_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let data_addr = data_listener.local_addr().unwrap();

        let data_shutdown = shutdown.child_token();
        tokio::spawn(async move {
            let _ = Server::builder()
                .add_service(ReplicatorDataServer::new(data_receiver))
                .serve_with_incoming_shutdown(
                    tokio_stream::wrappers::TcpListenerStream::new(data_listener),
                    data_shutdown.cancelled(),
                )
                .await;
        });

        // Control plane gRPC server
        let control_server = ControlServer::new(id, channels.control_tx.clone(), state.clone());
        let control_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let control_addr = control_listener.local_addr().unwrap();

        let ctrl_shutdown = shutdown.child_token();
        tokio::spawn(async move {
            let _ = Server::builder()
                .add_service(ReplicatorControlServer::new(control_server))
                .serve_with_incoming_shutdown(
                    tokio_stream::wrappers::TcpListenerStream::new(control_listener),
                    ctrl_shutdown.cancelled(),
                )
                .await;
        });

        // Replicator actor — keep data_tx alive in a background task
        let state_cp = state.clone();
        let actor = WalReplicatorActor::new(id);
        let data_tx_holder = channels.data_tx;
        let control_rx = channels.control_rx;
        let data_rx = channels.data_rx;
        tokio::spawn(async move {
            let _keep = data_tx_holder; // prevent data_rx from seeing closed channel
            actor.run(control_rx, data_rx, state_cp).await;
        });

        // Give servers a moment to start
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let control_address = format!("http://{}", control_addr);
        let data_address = format!("http://{}", data_addr);
        (control_address, data_address, shutdown)
    }

    /// End-to-end test: create 3-replica partition over gRPC, verify status, delete.
    #[tokio::test]
    async fn test_grpc_e2e_create_delete() {
        let (ctrl1, data1, shutdown1) = spawn_pod(1).await;
        let (ctrl2, data2, shutdown2) = spawn_pod(2).await;
        let (ctrl3, data3, shutdown3) = spawn_pod(3).await;

        let h1 = GrpcReplicaHandle::connect(1, ctrl1, data1).await.unwrap();
        let h2 = GrpcReplicaHandle::connect(2, ctrl2, data2).await.unwrap();
        let h3 = GrpcReplicaHandle::connect(3, ctrl3, data3).await.unwrap();

        let handles: Vec<Box<dyn crate::driver::ReplicaHandle>> =
            vec![Box::new(h1), Box::new(h2), Box::new(h3)];

        let mut driver = PartitionDriver::new();
        driver.create_partition(handles).await.unwrap();

        let pid = driver.primary_id().unwrap();
        assert!(pid > 0);

        driver.delete_partition().await.unwrap();

        shutdown1.cancel();
        shutdown2.cancel();
        shutdown3.cancel();
    }

    /// End-to-end failover test over gRPC.
    #[tokio::test]
    async fn test_grpc_e2e_failover() {
        let (ctrl1, data1, shutdown1) = spawn_pod(1).await;
        let (ctrl2, data2, shutdown2) = spawn_pod(2).await;
        let (ctrl3, data3, shutdown3) = spawn_pod(3).await;

        let h1 = GrpcReplicaHandle::connect(1, ctrl1, data1).await.unwrap();
        let h2 = GrpcReplicaHandle::connect(2, ctrl2, data2).await.unwrap();
        let h3 = GrpcReplicaHandle::connect(3, ctrl3, data3).await.unwrap();

        let handles: Vec<Box<dyn crate::driver::ReplicaHandle>> =
            vec![Box::new(h1), Box::new(h2), Box::new(h3)];

        let mut driver = PartitionDriver::new();
        driver.create_partition(handles).await.unwrap();

        let old_primary = driver.primary_id().unwrap();

        // Simulate primary failure
        shutdown1.cancel();
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        driver.failover(old_primary).await.unwrap();

        let new_primary = driver.primary_id().unwrap();
        assert_ne!(new_primary, old_primary);
        assert!(driver.handle(new_primary).is_some());

        driver.delete_partition().await.unwrap();
        shutdown2.cancel();
        shutdown3.cancel();
    }
}
