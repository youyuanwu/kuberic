use std::time::Duration;

use anyhow::{bail, ensure, Context, Result};
use async_trait::async_trait;
use kuberic_fm::{Phase, State, Status};
use tokio::{
    net::TcpListener,
    sync::{oneshot, watch, Mutex},
    task::JoinHandle,
};

use crate::{Allocation, BackendAdmission, FrozenCohort};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FmObservation {
    pub cohort: FrozenCohort,
    pub status: Status,
    pub joined: bool,
}

impl FmObservation {
    pub(crate) fn primary_index(&self) -> Result<usize> {
        ensure!(
            self.status.deployment == self.cohort.allocation.deployment,
            "native status changed the original deployment"
        );
        self.cohort
            .allocation
            .nodes
            .iter()
            .position(|node| node.node_id == self.status.primary_node_id)
            .context("native status selected a foreign primary")
    }
}

#[async_trait]
pub trait FmApi: Send + Sync {
    /// Reserve only before first allocation persistence and resource creation.
    async fn reserve(&self, allocation: &Allocation) -> Result<()>;
    async fn allocation(&self) -> Result<Option<Allocation>>;
    async fn observe(&self) -> Result<Option<FmObservation>>;
    async fn start(&self, cohort: &FrozenCohort) -> Result<()>;
    async fn shutdown(&self, cohort: &FrozenCohort) -> Result<()>;
    async fn join(&self, cohort: &FrozenCohort) -> Result<()>;
    async fn invalidate(&self, cohort: &FrozenCohort, reason: &str) -> Result<()>;
}

struct Session {
    cohort: FrozenCohort,
    status: watch::Receiver<Status>,
    task: Option<JoinHandle<Result<()>>>,
    shutdown: Option<oneshot::Sender<()>>,
    invalidation: Option<oneshot::Sender<String>>,
    failure: Option<String>,
}

/// One original FM session. The caller must shut down its nodes and explicitly
/// join this owner; dropping an observer is not a physical cleanup receipt.
#[must_use]
pub struct ProcessFm {
    admission: BackendAdmission,
    session: Mutex<Option<Session>>,
    listener: Mutex<Option<TcpListener>>,
}

impl Default for ProcessFm {
    fn default() -> Self {
        Self::new()
    }
}

impl ProcessFm {
    pub fn new() -> Self {
        Self {
            admission: BackendAdmission::default(),
            session: Mutex::new(None),
            listener: Mutex::new(None),
        }
    }

    pub fn with_listener(listener: TcpListener) -> Self {
        Self {
            listener: Mutex::new(Some(listener)),
            ..Self::new()
        }
    }

    pub async fn shutdown(&self, cohort: &FrozenCohort) -> Result<()> {
        let mut session = self.session.lock().await;
        let session = session.as_mut().context("FM has not started")?;
        ensure!(
            session.cohort == *cohort,
            "shutdown targets a different cohort"
        );
        ensure!(
            session.failure.is_none(),
            "FM authority is already invalidated"
        );
        if let Some(shutdown) = session.shutdown.take() {
            if shutdown.send(()).is_err() {
                let error = "native FM shutdown receiver closed";
                session.failure = Some(error.into());
                bail!("{error}");
            }
        }
        Ok(())
    }

    pub async fn join(&self) -> Result<()> {
        let mut session = self.session.lock().await;
        let Some(session) = session.as_mut() else {
            return Ok(());
        };
        if let Some(task) = &mut session.task {
            let result = tokio::time::timeout(Duration::from_secs(45), task)
                .await
                .context("FM join timed out; original task ownership is retained")?;
            session.task = None;
            match result {
                Ok(Ok(())) => {}
                Ok(Err(error)) => session.failure = Some(format!("{error:#}")),
                Err(error) => session.failure = Some(format!("FM task join failed: {error}")),
            }
        }
        if let Some(error) = &session.failure {
            bail!("{error}");
        }
        ensure!(
            session.status.borrow().phase == Phase::Stopped,
            "FM exited without native shutdown completion"
        );
        Ok(())
    }
}

#[async_trait]
impl FmApi for ProcessFm {
    async fn reserve(&self, allocation: &Allocation) -> Result<()> {
        self.admission.reserve(allocation).await
    }

    async fn allocation(&self) -> Result<Option<Allocation>> {
        Ok(self.admission.allocation().await)
    }

    async fn observe(&self) -> Result<Option<FmObservation>> {
        let mut session = self.session.lock().await;
        let Some(session) = session.as_mut() else {
            return Ok(None);
        };
        if session.task.as_ref().is_some_and(JoinHandle::is_finished) {
            let task = session.task.take().context("missing finished FM task")?;
            match task.await {
                Ok(Ok(())) if session.status.borrow().phase == Phase::Stopped => {}
                Ok(Ok(())) => {
                    session.failure = Some("native FM exited without shutdown completion".into())
                }
                Ok(Err(error)) => session.failure = Some(format!("{error:#}")),
                Err(error) => session.failure = Some(format!("FM task failed: {error}")),
            }
        }
        let mut status = session.status.borrow().clone();
        if let Some(error) = &session.failure {
            status.phase = Phase::ControlUnknown;
            status.service_endpoint = None;
            status.replication_endpoint = None;
            status.error = Some(error.clone());
        }
        Ok(Some(FmObservation {
            cohort: session.cohort.clone(),
            status,
            joined: session.task.is_none(),
        }))
    }

    async fn shutdown(&self, cohort: &FrozenCohort) -> Result<()> {
        ProcessFm::shutdown(self, cohort).await
    }

    async fn join(&self, cohort: &FrozenCohort) -> Result<()> {
        {
            let session = self.session.lock().await;
            ensure!(
                session
                    .as_ref()
                    .is_some_and(|session| session.cohort == *cohort),
                "FM join targets a different or absent original cohort"
            );
        }
        ProcessFm::join(self).await
    }

    async fn start(&self, cohort: &FrozenCohort) -> Result<()> {
        self.admission.require(&cohort.allocation).await?;
        let mut session = self.session.lock().await;
        if let Some(existing) = session.as_ref() {
            ensure!(
                existing.cohort == *cohort,
                "one FM owner cannot adopt another cohort"
            );
            ensure!(
                existing.failure.is_none()
                    && existing
                        .task
                        .as_ref()
                        .is_some_and(|task| !task.is_finished())
                    && !matches!(
                        existing.status.borrow().phase,
                        Phase::ControlUnknown | Phase::Stopped
                    ),
                "original FM session is terminal; restart is unsupported"
            );
            return Ok(());
        }
        let config = cohort.fm_config()?;
        let mut reserved = self.listener.lock().await;
        let listener = match reserved.as_ref() {
            Some(listener) => {
                ensure!(
                    listener.local_addr()? == config.fm.address.into(),
                    "reserved listener differs from accepted FM address"
                );
                reserved.take().context("missing reserved FM listener")?
            }
            None => TcpListener::bind(config.fm.address)
                .await
                .context("binding accepted FM listener")?,
        };
        drop(reserved);
        let (status, observed) = watch::channel(State::new(config.clone())?.status());
        let (shutdown, stopped) = oneshot::channel();
        let (invalidation, invalidated) = oneshot::channel();
        let task = tokio::spawn(kuberic_fm::run_guarded(
            config,
            listener,
            async {
                stopped.await.context("FM shutdown owner dropped")?;
                Ok(())
            },
            async {
                match invalidated.await {
                    Ok(reason) => reason,
                    Err(_) => "live FM owner dropped".into(),
                }
            },
            move |value| {
                status.send_replace(value.clone());
                Ok(())
            },
        ));
        *session = Some(Session {
            cohort: cohort.clone(),
            status: observed,
            task: Some(task),
            shutdown: Some(shutdown),
            invalidation: Some(invalidation),
            failure: None,
        });
        Ok(())
    }

    async fn invalidate(&self, cohort: &FrozenCohort, reason: &str) -> Result<()> {
        let mut session = self.session.lock().await;
        let Some(session) = session.as_mut() else {
            return Ok(());
        };
        ensure!(
            session.cohort == *cohort,
            "invalidation targets a different cohort"
        );
        if session.failure.is_none() {
            session.failure = Some(reason.into());
        }
        if let Some(invalidation) = session.invalidation.take() {
            invalidation.send(reason.into()).map_err(|_| {
                anyhow::anyhow!("native FM invalidation receiver is already closed")
            })?;
        }
        Ok(())
    }
}
