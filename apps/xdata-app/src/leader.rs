use std::time::Duration;

use kube_leader_election::{LeaseLock, LeaseLockParams, LeaseLockResult};
use rand::{Rng, distr::Alphanumeric};
use tokio_util::sync::CancellationToken;

pub type Epoch = i32;

#[derive(Debug)]
pub enum LeaderElectionEvent {
    Leader(Epoch),
    StandBy(Epoch),
    Shutdown(Result<(), kube_leader_election::Error>),
}

#[derive(Clone, Debug)]
pub struct LeaderElection {
    pub namespace: String,
    pub lease_name: String,
    pub lease_ttl: Duration,
}

impl LeaderElection {
    pub fn new(namespace: String, lease_name: String, lease_ttl: Duration) -> Self {
        LeaderElection {
            namespace,
            lease_name,
            lease_ttl,
        }
    }
    pub async fn election_loop(
        self,
        sender: tokio::sync::mpsc::Sender<LeaderElectionEvent>,
        token: CancellationToken,
    ) {
        let client = kube::Client::try_default().await.unwrap();

        // random id part for the sake of simulating something like a pod hash
        let random: String = rand::rng()
            .sample_iter(&Alphanumeric)
            .take(7)
            .map(char::from)
            .collect();
        let holder_id = format!("shared-lease-{}", random.to_lowercase());
        tracing::info!("Starting leader election with holder id {}", holder_id);
        let leadership = LeaseLock::new(
            client,
            &self.namespace,
            LeaseLockParams {
                holder_id: holder_id.clone(),
                lease_name: self.lease_name.clone(),
                lease_ttl: self.lease_ttl,
            },
        );
        let mut cur_epoch: Epoch = 0;
        let mut on_new_leader = async |ll: LeaseLockResult| {
            if ll.acquired_lease {
                let l = ll.lease.unwrap();
                let epoch = l.spec.unwrap().lease_transitions.unwrap();
                tracing::info!("Acquired leadership {} for epoch {}", holder_id, epoch);
                assert!(epoch >= cur_epoch, "Epoch should not go backwards");
                cur_epoch = epoch;
                sender
                    .send(LeaderElectionEvent::Leader(epoch))
                    .await
                    .unwrap();
            } else {
                tracing::info!("No lease acquired, standing by");
                // There is no lease acquired
                // TODO: Make a kube call to get the current lease state
                sender
                    .send(LeaderElectionEvent::StandBy(cur_epoch))
                    .await
                    .unwrap();
            }
        };

        loop {
            let start_time = tokio::time::Instant::now();
            tokio::select! {
                result = leadership.try_acquire_or_renew() => {
                    match result {
                        Ok(ll) => {
                            on_new_leader(ll).await;
                        },
                        Err(err) => tracing::error!("Leader election error {:?}", err),
                    }
                }
                _ = token.cancelled() => {
                    tracing::info!("Leader election loop received shutdown signal");
                    break;
                }
            }
            // sleep before next renew attempt
            // should be the smaller of the half of lease ttl or time left until half ttl
            let elapsed = start_time.elapsed();
            let sleep_threshold = self.lease_ttl / 2;
            if elapsed >= sleep_threshold {
                continue;
            }
            tokio::select! {
                _ = tokio::time::sleep(sleep_threshold) => {},
                _ = token.cancelled() => {
                    tracing::info!("Leader election loop received shutdown signal");
                    break;
                }
            }
        }
        // step down as leader
        sender
            .send(LeaderElectionEvent::Shutdown(leadership.step_down().await))
            .await
            .unwrap();
        tracing::info!("Leader election loop exited");
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use xedio_shared::XEDIO_TEST_NAMESPACE;

    pub mod helper {

        use crate::leader::LeaderElection;

        pub struct LETestReceiver {
            pub rx: tokio::sync::mpsc::Receiver<super::LeaderElectionEvent>,
            pub token: tokio_util::sync::CancellationToken,
            pub join_handle: tokio::task::JoinHandle<()>,
        }
        impl LETestReceiver {
            pub fn new(le: LeaderElection) -> Self {
                let (tx, rx) = tokio::sync::mpsc::channel(1);
                let token = tokio_util::sync::CancellationToken::new();
                let token_cp = token.clone();
                let join_handle = tokio::spawn(async move {
                    le.election_loop(tx, token_cp).await;
                });
                LETestReceiver {
                    rx,
                    token,
                    join_handle,
                }
            }

            pub async fn must_receive_event(&mut self) -> super::LeaderElectionEvent {
                self.rx.recv().await.unwrap()
            }

            pub async fn shutdown(mut self) {
                self.token.cancel();
                self.join_handle.await.unwrap();
                // get the shutdown event
                let e = self.rx.recv().await.unwrap();
                matches!(e, super::LeaderElectionEvent::Shutdown(Ok(())));
            }
        }
    }

    #[tokio::test]
    #[test_log::test]
    async fn test_leader_election_new() {
        xedio_shared::kube_util::init_test_namespace().await;
        let le = LeaderElection::new(
            XEDIO_TEST_NAMESPACE.to_string(),
            "test-lease1".to_string(),
            Duration::from_secs(2),
        );
        let token = CancellationToken::new();
        let token_cp = token.clone();
        let (sender, mut receiver) = tokio::sync::mpsc::channel(1);
        tokio::spawn(async move {
            le.election_loop(sender, token_cp).await;
        });
        let ev = receiver
            .recv()
            .await
            .expect("Should receive at least one election event");
        matches!(ev, LeaderElectionEvent::Leader(0));

        // stop the election loop
        token.cancel();
        // get the shutdown event
        let ev = receiver
            .recv()
            .await
            .expect("Should receive shutdown event");
        matches!(ev, LeaderElectionEvent::Shutdown(Ok(())));
    }

    #[tokio::test]
    #[test_log::test]
    #[ignore = "Has race conditions, needs to be fixed"]
    async fn test_change_leader() {
        xedio_shared::kube_util::init_test_namespace().await;

        let lease_name = "test-lease2".to_string();
        let le1 = LeaderElection::new(
            XEDIO_TEST_NAMESPACE.to_string(),
            lease_name.clone(),
            Duration::from_secs(2),
        );
        let le2 = LeaderElection::new(
            XEDIO_TEST_NAMESPACE.to_string(),
            lease_name.clone(),
            Duration::from_secs(2),
        );

        let mut receiver1 = helper::LETestReceiver::new(le1);
        let mut receiver2 = helper::LETestReceiver::new(le2);
        // First receiver should become leader
        let ev1 = receiver1.must_receive_event().await;
        let leader_epoch = if let LeaderElectionEvent::Leader(epoch) = ev1 {
            epoch
        } else {
            panic!("First receiver should become leader: got {:?}", ev1);
        };
        // Second receiver should be standby
        let ev2 = receiver2.must_receive_event().await;
        let standby_epoch = if let LeaderElectionEvent::StandBy(epoch) = ev2 {
            epoch
        } else {
            panic!("Second receiver should be standby: got {:?}", ev2);
        };
        assert_eq!(leader_epoch, standby_epoch);
        // Shutdown first receiver
        receiver1.shutdown().await;
        // Second receiver should become leader now
        let ev2 = receiver2.must_receive_event().await;
        let leader_epoch2 = if let LeaderElectionEvent::Leader(epoch) = ev2 {
            epoch
        } else {
            panic!("Second receiver should become leader: got {:?}", ev2);
        };
        assert_eq!(leader_epoch2, leader_epoch + 1);
    }
}
