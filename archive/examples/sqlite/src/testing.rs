use std::time::Duration;

#[derive(Default)]
pub struct PauseGate {
    armed: std::sync::atomic::AtomicBool,
    entered: tokio::sync::Notify,
    released: tokio::sync::Notify,
}

impl PauseGate {
    pub fn arm(&self) {
        assert!(!self.armed.swap(true, std::sync::atomic::Ordering::SeqCst));
    }

    pub async fn wait_entered(&self) {
        tokio::time::timeout(Duration::from_secs(5), self.entered.notified())
            .await
            .expect("application reached the requested durable cut");
    }

    pub fn release(&self) {
        self.released.notify_one();
    }

    pub(crate) async fn pause(&self) {
        if self.armed.swap(false, std::sync::atomic::Ordering::SeqCst) {
            self.entered.notify_one();
            self.released.notified().await;
        }
    }
}
