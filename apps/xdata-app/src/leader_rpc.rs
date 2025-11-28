use std::sync::{Arc, atomic::Ordering};

// gRPC service implementation
use tonic::{Request, Response, Status};

pub use hello_world::leader_election_server::{
    LeaderElection as LeaderElectionService, LeaderElectionServer,
};
use hello_world::{IsLeaderReply, IsLeaderRequest};
use xedio_shared::proto::hello_world;

#[derive(Debug, Clone)]
pub struct MyLeaderElection {
    app_state: Arc<crate::app::AppState>,
}

impl MyLeaderElection {
    pub fn new(app_state: Arc<crate::app::AppState>) -> Self {
        Self { app_state }
    }
}

#[tonic::async_trait]
impl LeaderElectionService for MyLeaderElection {
    async fn is_leader(
        &self,
        _request: Request<IsLeaderRequest>,
    ) -> Result<Response<IsLeaderReply>, Status> {
        // Get pod name from environment variable (set by Kubernetes)
        let pod_name = std::env::var("HOSTNAME").unwrap_or_else(|_| "unknown".to_string());

        let is_leader = self.app_state.is_leader.load(Ordering::Relaxed);
        let lease_transitions = self.app_state.lease_transitions.load(Ordering::Relaxed);

        tracing::debug!(
            "Leader status check: pod={}, is_leader={}",
            pod_name,
            is_leader
        );

        let reply = IsLeaderReply {
            is_leader,
            leader_id: pod_name,
            lease_transitions: lease_transitions as u32,
        };

        Ok(Response::new(reply))
    }
}
