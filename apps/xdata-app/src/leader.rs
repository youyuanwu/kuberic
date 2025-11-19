use std::time::Duration;

use kube_leader_election::{LeaseLock, LeaseLockParams, LeaseLockResult};
use rand::{Rng, distr::Alphanumeric};
use tokio_util::sync::CancellationToken;

pub type Epoch = i32;

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

        let leadership = LeaseLock::new(
            client,
            &self.namespace,
            LeaseLockParams {
                holder_id,
                lease_name: self.lease_name.clone(),
                lease_ttl: self.lease_ttl,
            },
        );
        let mut cur_epoch: Epoch = 0;
        let mut on_new_leader = async |ll: LeaseLockResult| {
            tracing::info!("Leader election try acquire or renew: {:?}", ll);
            if ll.acquired_lease {
                let l = ll.lease.unwrap();
                let epoch = l.spec.unwrap().lease_transitions.unwrap();
                assert!(epoch >= cur_epoch, "Epoch should not go backwards");
                cur_epoch = epoch;
                sender
                    .send(LeaderElectionEvent::Leader(epoch))
                    .await
                    .unwrap();
            } else {
                // There is no lease acquired
                // TODO: Make a kube call to get the current lease state

                sender
                    .send(LeaderElectionEvent::StandBy(cur_epoch))
                    .await
                    .unwrap();
            }
        };

        loop {
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
            tokio::select! {
                _ = tokio::time::sleep(self.lease_ttl / 2) => {},
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
mod tests {
    use super::*;
    use k8s_openapi::api::core::v1::Namespace;

    const TEST_NAMESPACE: &str = "xedio-test-ns";

    async fn create_test_namespace() {
        let client = kube::Client::try_default().await.unwrap();
        let namespaces = kube::Api::all(client);
        let ns = Namespace {
            metadata: kube::api::ObjectMeta {
                name: Some(TEST_NAMESPACE.to_string()),
                ..Default::default()
            },
            ..Default::default()
        };
        match namespaces
            .create(&kube::api::PostParams::default(), &ns)
            .await
        {
            Ok(_) => (),
            Err(kube::Error::Api(ae)) if ae.code == 409 => (), // already exists
            Err(e) => panic!("Failed to create test namespace: {}", e),
        }
    }

    #[tokio::test]
    #[test_log::test]
    async fn test_leader_election_new() {
        create_test_namespace().await;
        let le = LeaderElection::new(
            TEST_NAMESPACE.to_string(),
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
}
