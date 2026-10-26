use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use crate::instance::PgError;

struct Join {
    task: Option<tokio::task::JoinHandle<()>>,
    result: Option<Result<(), PgError>>,
}

type Observation = Pin<Box<dyn Future<Output = Result<(), PgError>> + Send>>;

/// An observer of a retained driver join, not ownership of the driver itself.
pub struct PgTask {
    join: Arc<tokio::sync::Mutex<Join>>,
    abort: tokio::task::AbortHandle,
    expected: Arc<AtomicBool>,
    observation: Option<Observation>,
}

impl Clone for PgTask {
    fn clone(&self) -> Self {
        Self {
            join: self.join.clone(),
            abort: self.abort.clone(),
            expected: self.expected.clone(),
            observation: None,
        }
    }
}

impl PgTask {
    pub(crate) fn new(task: tokio::task::JoinHandle<()>) -> Self {
        Self {
            abort: task.abort_handle(),
            join: Arc::new(tokio::sync::Mutex::new(Join {
                task: Some(task),
                result: None,
            })),
            expected: Arc::new(AtomicBool::new(false)),
            observation: None,
        }
    }

    pub fn abort(&self) {
        if !self.abort.is_finished() {
            self.expected.store(true, Ordering::Release);
            self.abort.abort();
        }
    }

    #[cfg(all(test, feature = "testing"))]
    pub(crate) async fn lose(&self) {
        self.abort.abort();
        assert!(self.clone().await.is_err());
    }

    fn capture(&self, result: Result<(), tokio::task::JoinError>) -> Result<(), PgError> {
        match result {
            Ok(()) => Ok(()),
            Err(error) if error.is_cancelled() && self.expected.load(Ordering::Acquire) => Ok(()),
            Err(error) => Err(PgError::Process(format!(
                "lost PostgreSQL Rust task: {error}"
            ))),
        }
    }

    pub(crate) fn settled_successfully(&self) -> bool {
        let Ok(mut join) = self.join.try_lock() else {
            return false;
        };
        if let Some(task) = join.task.as_mut()
            && task.is_finished()
        {
            let mut context = Context::from_waker(std::task::Waker::noop());
            if let Poll::Ready(result) = Pin::new(task).poll(&mut context) {
                join.result = Some(self.capture(result));
                join.task.take();
            }
        }
        matches!(join.result, Some(Ok(())))
    }
}

impl Future for PgTask {
    type Output = Result<(), PgError>;

    fn poll(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
        if self.observation.is_none() {
            let owner = self.clone();
            self.observation = Some(Box::pin(async move {
                let mut join = owner.join.lock().await;
                if join.result.is_none() {
                    let result = join.task.as_mut().expect("retained task").await;
                    join.result = Some(owner.capture(result));
                    join.task.take();
                }
                join.result.as_ref().expect("joined task").clone()
            }));
        }
        self.observation.as_mut().unwrap().as_mut().poll(context)
    }
}

#[derive(Default)]
pub(crate) struct PgTasks(Mutex<Vec<(u64, bool, PgTask)>>);

impl PgTasks {
    #[cfg(all(test, feature = "testing"))]
    pub(crate) async fn lose_monitor(&self, generation: u64) {
        let task = {
            let tasks = self.0.lock().unwrap();
            tasks
                .iter()
                .find(|(id, monitor, task)| {
                    *id == generation && *monitor && !task.abort.is_finished()
                })
                .expect("live native monitor")
                .2
                .clone()
        };
        task.abort.abort();
        assert!(task.await.is_err());
    }
    pub(crate) fn retain(
        &self,
        generation: u64,
        monitor: bool,
        task: tokio::task::JoinHandle<()>,
    ) -> PgTask {
        let task = PgTask::new(task);
        let mut tasks = self.0.lock().unwrap();
        tasks.retain(|(_, _, task)| !task.settled_successfully());
        tasks.push((generation, monitor, task.clone()));
        task
    }

    pub(crate) fn fence(&self, generation: u64) {
        for (id, monitor, task) in self.0.lock().unwrap().iter() {
            if *id == generation && *monitor {
                task.abort();
            }
        }
    }

    pub(crate) async fn join(&self, generation: Option<u64>) -> Result<(), PgError> {
        let tasks = self
            .0
            .lock()
            .unwrap()
            .iter()
            .filter(|(id, _, _)| generation.is_none_or(|expected| *id == expected))
            .map(|(_, _, task)| task.clone())
            .collect::<Vec<_>>();
        let mut failures = Vec::new();
        for task in tasks {
            if let Err(error) = task.await {
                failures.push(error.to_string());
            }
        }
        if failures.is_empty() {
            Ok(())
        } else {
            Err(PgError::Process(failures.join("; ")))
        }
    }
}
