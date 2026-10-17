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
