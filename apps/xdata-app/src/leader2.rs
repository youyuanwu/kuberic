#[cfg(test)]
mod tests {
    use std::{
        sync::{Arc, atomic::AtomicU32},
        time::Duration,
    };

    use kube::Client;
    use kube_lease_manager::LeaseManagerBuilder;
    use xedio_shared::XEDIO_TEST_NAMESPACE;

    #[tokio::test]
    #[test_log::test]
    async fn test_leader_election_basic() {
        xedio_shared::kube_util::init_test_namespace().await;
        // Use the default Kube client
        let client = Client::try_default().await.unwrap();
        // Create the simplest LeaseManager with reasonable defaults using a convenient builder.
        // It uses a Lease resource called `test-watch-lease`.
        let lease_name = "test-watch-lease";

        // make a vec of lease managers to simulate multiple instances
        let mut lease_managers = Vec::new();
        let count = 5;
        for i in 0..count {
            let manager = LeaseManagerBuilder::new(client.clone(), lease_name)
                .with_namespace(XEDIO_TEST_NAMESPACE)
                .with_duration(10)
                .with_grace(5)
                .with_create_mode(kube_lease_manager::LeaseCreateMode::AutoCreate)
                .with_identity(format!("instance-{}", i))
                .build()
                .await
                .unwrap();
            lease_managers.push(manager);
        }

        // start watching on all lease managers in parallel
        let mut join_set = tokio::task::JoinSet::new();
        let leader_count: Arc<AtomicU32> = Arc::new(AtomicU32::new(0));
        let standby_count: Arc<AtomicU32> = Arc::new(AtomicU32::new(0));
        // Use a barrier to synchronize the start of all tasks
        let barrier = Arc::new(tokio::sync::Barrier::new(count));
        for manager in lease_managers {
            let leader_count = leader_count.clone();
            let standby_count = standby_count.clone();
            let barrier = barrier.clone();
            join_set.spawn(async move {
                barrier.wait().await;
                let (mut channel, task) = manager.watch().await;
                // Watch on the channel for lock state changes
                tokio::select! {
                    _ = channel.changed() => {
                        let lock_state = *channel.borrow_and_update();

                        if lock_state {
                            // Do something useful as a leader
                            leader_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                            println!("Got a luck!");
                            tokio::time::sleep(Duration::from_secs(3)).await; // do some work
                        }else{
                            standby_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                            println!("Became standby");
                        }
                    }
                    _ = tokio::time::sleep(Duration::from_secs(1)) => {
                        println!("Unable to get lock during 1s");
                    }
                }

                // Explicitly close the control channel
                drop(channel);
                // Wait for the finish of the manager and get it back
                let manager = tokio::join!(task).0.unwrap().unwrap();
                manager.release().await.unwrap();
            });
        }
        // Wait for all tasks to complete
        while let Some(res) = join_set.join_next().await {
            res.unwrap();
        }
        assert_eq!(leader_count.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(standby_count.load(std::sync::atomic::Ordering::SeqCst), 0);
    }
}
