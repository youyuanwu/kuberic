use super::*;
use tokio::sync::{Notify, watch};

pub(crate) struct TerminalCleanup {
    requested: Notify,
    result: watch::Receiver<Option<std::result::Result<(), String>>>,
    contained: watch::Receiver<bool>,
}

impl TerminalCleanup {
    pub(super) fn witness(&self) -> watch::Receiver<bool> {
        self.contained.clone()
    }

    pub(super) async fn wait(&self) -> Result<()> {
        self.requested.notify_one();
        let mut result = self.result.clone();
        loop {
            if let Some(result) = result.borrow().clone() {
                return result.map_err(HostError::CommandRejected);
            }
            result.changed().await.map_err(|_| {
                HostError::CommandRejected("terminal cleanup owner lost before drain".into())
            })?;
        }
    }
}

pub(super) struct TerminalRootGuard(pub(super) PublicLifecycleCallbacks);

impl Drop for TerminalRootGuard {
    fn drop(&mut self) {
        let cleanup = self.0.terminal_cleanup.as_ref().expect("terminal cleanup");
        if cleanup.result.borrow().is_none() {
            self.0.abort_once();
            cleanup.requested.notify_one();
        }
    }
}

pub(super) fn bind(
    callbacks: PublicLifecycleCallbacks,
) -> (
    Arc<TerminalCleanup>,
    impl std::future::Future<Output = ()> + Send + 'static,
) {
    let (result, result_rx) = watch::channel(None);
    let (contained, contained_rx) = watch::channel(false);
    let cleanup = Arc::new(TerminalCleanup {
        requested: Notify::new(),
        result: result_rx,
        contained: contained_rx.clone(),
    });
    let owner = cleanup.clone();
    let task = async move {
        owner.requested.notified().await;
        let mut failures = Vec::new();
        if let Err(error) = (callbacks.terminal_drain)().await {
            failures.push(error.to_string());
        }
        if !failures.is_empty()
            && callbacks
                .containment
                .as_ref()
                .is_some_and(|witness| !*witness.borrow())
        {
            // A failed custom drain can still own live descendants. Retain its
            // failure independently; returning is not a positive join witness.
            result.send_replace(Some(Err(failures.join("; "))));
            return;
        }
        if let Some(mut witness) = callbacks.containment
            && witness.wait_for(|contained| *contained).await.is_err()
        {
            // A lost callback witness is not proof of containment.
            failures.push("terminal containment witness lost".into());
            result.send_replace(Some(Err(failures.join("; "))));
            return;
        }
        contained.send_replace(true);
        result.send_replace(Some(if failures.is_empty() {
            Ok(())
        } else {
            Err(failures.join("; "))
        }));
    };
    (cleanup, task)
}

pub(crate) async fn recover(
    registry: &PartitionOperationRegistry,
    operation: &Arc<PartitionOperation>,
    mut callbacks: PublicLifecycleCallbacks,
) -> Result<()> {
    let (contained, witness) = watch::channel(false);
    if !operation.track_recovery_containment(witness).await {
        return Ok(());
    }
    registry.fence();
    callbacks.aborted = registry.abort_guard();
    // Recovery cannot execute the uncertain instruction program. It still
    // owns Abort and actual drain, independently of any replay observer.
    let revoked = (callbacks.revoke_access)();
    callbacks.abort_once();
    let (cleanup, owner) = bind(callbacks);
    let operation = operation.clone();
    registry.own_terminal_task(async move {
        let join = async {
            let result = cleanup.wait().await.and(revoked.map_err(HostError::from));
            match result {
                Ok(()) => {
                    contained.send_replace(true);
                    if let Err(error) = operation.complete_containment().await {
                        operation.record_containment_failure(&error);
                    }
                }
                Err(error) => {
                    // Keep the original Ambiguous/Cancelled disposition and a
                    // separate retained cleanup diagnostic. Do not settle on error.
                    operation.record_containment_failure(&error);
                }
            }
        };
        tokio::join!(owner, join);
    });
    Ok(())
}
