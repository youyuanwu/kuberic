use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicI32},
};

use tokio_util::sync::CancellationToken;

use crate::leader::LeaderElection;

#[derive(Debug)]
pub struct App {
    leader_election: LeaderElection,
    state: Arc<AppState>,
}

#[derive(Debug)]
pub struct AppState {
    pub is_leader: AtomicBool,
    pub lease_transitions: AtomicI32,
}

impl App {
    pub fn new(leader_election: LeaderElection) -> (Self, Arc<AppState>) {
        let state = Arc::new(AppState {
            is_leader: AtomicBool::new(false),
            lease_transitions: AtomicI32::new(0),
        });
        (
            Self {
                leader_election,
                state: state.clone(),
            },
            state,
        )
    }

    pub async fn run(self, token: CancellationToken) {
        let (sender, mut receiver) = tokio::sync::mpsc::channel(1);
        let (leader_election, state) = (self.leader_election, self.state);
        // Spawn leader election loop
        let le_task = tokio::spawn(async move {
            leader_election.election_loop(sender, token).await;
        });

        // Receives events and apply to state
        while let Some(event) = receiver.recv().await {
            match event {
                crate::leader::LeaderElectionEvent::Leader(epoch) => {
                    tracing::info!("Became leader at epoch {}", epoch);
                    state
                        .is_leader
                        .store(true, std::sync::atomic::Ordering::Relaxed);
                    state
                        .lease_transitions
                        .store(epoch, std::sync::atomic::Ordering::Relaxed);
                }
                crate::leader::LeaderElectionEvent::StandBy(epoch) => {
                    tracing::info!("In standby mode at epoch {}", epoch);
                    state
                        .is_leader
                        .store(false, std::sync::atomic::Ordering::Relaxed);
                    state
                        .lease_transitions
                        .store(epoch, std::sync::atomic::Ordering::Relaxed);
                }
                crate::leader::LeaderElectionEvent::Shutdown(res) => {
                    tracing::info!("Shutting down leader election: {:?}", res);
                    break;
                }
            }
        }
        le_task.await.unwrap();
    }
}
