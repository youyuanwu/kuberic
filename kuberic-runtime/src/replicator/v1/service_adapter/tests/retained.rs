use super::super::facade::Facade;
use super::facade::{facade_config, until};
use super::*;
use crate::protocol::types::ReplicaRole;
use crate::replicator::Replicator;

#[tokio::test]
async fn retained_facade_drives_real_provider_and_retries_only_exact_initialized_epoch() {
    let mut node = Node::new(2, true, false, Bus::default(), MessageRegistry::default());
    node.provider.progress.store(7, Ordering::SeqCst);
    node.provider.epoch_gate.forget_permits(100);
    let provider = node.provider.clone();
    let config = facade_config(&node, &[]);
    node.adapter.open().await.unwrap();
    let facade = Facade::start(node.adapter, config).unwrap();
    let call = facade.clone();
    let role = tokio::spawn(async move { call.restore_retained_secondary(epoch(1)).await });
    until(|| !provider.epochs.lock().unwrap().is_empty()).await;
    assert_eq!(provider.progress_calls.load(Ordering::SeqCst), 1);
    assert!(!role.is_finished());
    provider.epoch_gate.add_permits(1);
    role.await.unwrap().unwrap();
    assert_eq!(facade.current_progress().await.unwrap(), 7);
    assert_eq!(facade.catch_up_capability().await.unwrap(), 0);
    assert!(
        facade
            .service()
            .copy_stream()
            .unwrap()
            .next()
            .await
            .is_none()
    );
    assert!(
        facade
            .service()
            .replication_stream()
            .unwrap()
            .next()
            .now_or_never()
            .is_none()
    );
    facade.restore_retained_secondary(epoch(1)).await.unwrap();
    assert!(facade.restore_retained_secondary(epoch(2)).await.is_err());
    assert_eq!(provider.progress_calls.load(Ordering::SeqCst), 1);
    assert_eq!(*provider.epochs.lock().unwrap(), vec![(epoch(1), 7)]);
    facade.abort();
    facade.drain().await.unwrap();
}

#[tokio::test]
async fn retained_facade_failed_epoch_has_no_receiver_or_successful_retry() {
    let node = Node::new(2, true, false, Bus::default(), MessageRegistry::default());
    let provider = node.provider.clone();
    provider.fail_epoch.store(true, Ordering::SeqCst);
    let config = facade_config(&node, &[]);
    let facade = Facade::start(node.adapter, config).unwrap();
    facade.open().await.unwrap();
    assert!(facade.restore_retained_secondary(epoch(1)).await.is_err());
    assert!(facade.service().replication_stream().is_err());
    assert!(facade.current_progress().await.is_err());
    assert!(facade.restore_retained_secondary(epoch(1)).await.is_err());
    assert_eq!(provider.progress_calls.load(Ordering::SeqCst), 1);
    facade.abort();
    let _ = facade.drain().await;
}

#[tokio::test]
async fn retained_facade_cancelled_waiter_keeps_the_actual_constructor_and_provider_callback() {
    let node = Node::new(2, true, false, Bus::default(), MessageRegistry::default());
    let provider = node.provider.clone();
    provider.progress.store(7, Ordering::SeqCst);
    provider.epoch_gate.forget_permits(100);
    let config = facade_config(&node, &[]);
    let facade = Facade::start(node.adapter, config).unwrap();
    facade.open().await.unwrap();
    let caller = facade.clone();
    let selected = tokio::spawn(async move { caller.restore_retained_secondary(epoch(1)).await });
    until(|| !provider.epochs.lock().unwrap().is_empty()).await;
    selected.abort();
    assert!(selected.await.unwrap_err().is_cancelled());
    provider.epoch_gate.add_permits(1);
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if facade.current_progress().await.is_ok() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    facade.restore_retained_secondary(epoch(1)).await.unwrap();
    assert_eq!(provider.progress_calls.load(Ordering::SeqCst), 1);
    assert_eq!(*provider.epochs.lock().unwrap(), vec![(epoch(1), 7)]);
    facade.abort();
    facade.drain().await.unwrap();
}

#[tokio::test]
async fn ordinary_existing_secondary_still_requires_remote_copy_and_cannot_claim_retained_mode() {
    let node = Node::new(2, true, false, Bus::default(), MessageRegistry::default());
    let config = facade_config(&node, &[]);
    let facade = Facade::start(node.adapter, config).unwrap();
    facade.open().await.unwrap();
    facade
        .change_role(epoch(1), ReplicaRole::ActiveSecondary)
        .await
        .unwrap();
    assert!(facade.current_progress().await.is_err());
    assert!(facade.restore_retained_secondary(epoch(1)).await.is_err());
    assert!(
        facade
            .service()
            .copy_stream()
            .unwrap()
            .next()
            .now_or_never()
            .is_none()
    );
    facade.abort();
    facade.drain().await.unwrap();
}

#[cfg(feature = "testing")]
mod production;
