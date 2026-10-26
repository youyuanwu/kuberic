// Copyright (c) Microsoft Corporation. All rights reserved.
// Licensed under the MIT License. See LICENSES/service-fabric-MIT.txt.
//
// Service-owned ordering from Store/ReplicatedStore.TransactionReplicator.cpp:
// 606-796,848-900, microsoft/service-fabric@3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d.

//! Unselected KV service preparation, not another replicator or factory.
//! New-instance recovery discards only locally uncommitted prepares. A dropped
//! client waiter does not cancel work; after admission, failure is ambiguous.
//! The caller drives native roles/streams and supplies actual host access/fault
//! callbacks. The repository testing bridge exposes no private engine types.

use std::future::Future;
use std::io;
use std::path::Path;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use tokio::sync::{Mutex as AsyncMutex, Notify, mpsc, oneshot, watch};
use tokio::task::JoinHandle;

use crate::persistence::KvPersistence;

pub struct Assignment {
    pub lsn: i64,
    pub completion: Pin<Box<dyn Future<Output = io::Result<()>> + Send>>,
}

pub struct Write {
    pub lsn: i64,
    outcome: oneshot::Receiver<io::Result<i64>>,
    result: Option<Result<i64, (bool, String)>>,
}

impl Write {
    pub async fn wait(&mut self) -> io::Result<i64> {
        if self.result.is_none() {
            let result = match (&mut self.outcome).await {
                Ok(result) => result,
                Err(_) => Err(io::Error::other(
                    "KV commit worker abandoned client outcome",
                )),
            };
            self.result = Some(result.map_err(|error| (is_abort(&error), error.to_string())));
        }
        self.result
            .as_ref()
            .unwrap()
            .as_ref()
            .copied()
            .map_err(|(aborted, reason)| stored_error(*aborted, reason.clone()))
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    None,
    Primary,
    Copy { installed: bool },
    Secondary,
}

struct Pending {
    assignment: Assignment,
    reply: oneshot::Sender<io::Result<i64>>,
}

struct State {
    sender: Option<mpsc::UnboundedSender<Pending>>,
    fault: Option<String>,
    aborted: bool,
    mode: Mode,
    pending: usize,
    #[cfg(feature = "testing")]
    commit_pause: Option<CommitPause>,
}

struct Drain {
    worker: Option<JoinHandle<io::Result<()>>>,
    result: Option<Result<(), (bool, String)>>,
}

pub struct Committer {
    store: Arc<KvPersistence>,
    state: Arc<Mutex<State>>,
    changed: Arc<Notify>,
    cancel: watch::Sender<bool>,
    drain: AsyncMutex<Drain>,
    report: Arc<dyn Fn(String) + Send + Sync>,
}

fn error(error: impl std::fmt::Display) -> io::Error {
    io::Error::other(error.to_string())
}

#[derive(Debug)]
struct Aborted(String);

impl std::fmt::Display for Aborted {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for Aborted {}

pub(crate) fn is_abort(error: &io::Error) -> bool {
    error.get_ref().is_some_and(|cause| {
        cause.is::<Aborted>()
            || matches!(
                cause.downcast_ref::<kuberic_runtime::RuntimeError>(),
                Some(kuberic_runtime::RuntimeError::OperationCancelled)
            )
    })
}

fn stored_error(aborted: bool, reason: String) -> io::Error {
    if aborted {
        io::Error::new(io::ErrorKind::Interrupted, Aborted(reason))
    } else {
        io::Error::other(reason)
    }
}

fn live(state: &State) -> io::Result<()> {
    if let Some(error) = fault_error(state) {
        return Err(error);
    }
    if state
        .sender
        .as_ref()
        .is_none_or(|sender| sender.is_closed())
    {
        return Err(io::Error::other("KV service is closing"));
    }
    Ok(())
}

fn fault_error(state: &State) -> Option<io::Error> {
    state
        .fault
        .as_ref()
        .map(|reason| stored_error(state.aborted, reason.clone()))
}

fn record_fault(state: &mut State, reason: &str) -> bool {
    let first = state.fault.is_none();
    if first {
        state.fault = Some(reason.to_owned());
    }
    state.sender.take();
    first
}

fn publish_fault(changed: &Notify, report: &dyn Fn(String), reason: String, first: bool) {
    changed.notify_waiters();
    if first {
        report(reason);
    }
}

fn fence(state: &Mutex<State>, changed: &Notify, report: &dyn Fn(String), reason: String) {
    let first = record_fault(&mut state.lock().unwrap(), &reason);
    publish_fault(changed, report, reason, first);
}

impl Committer {
    pub fn open(
        root: impl AsRef<Path>,
        report: Arc<dyn Fn(String) + Send + Sync>,
    ) -> io::Result<Self> {
        let store = Arc::new(KvPersistence::open(root).map_err(error)?);
        Self::with_store(store, report)
    }

    pub(crate) fn with_store(
        store: Arc<KvPersistence>,
        report: Arc<dyn Fn(String) + Send + Sync>,
    ) -> io::Result<Self> {
        store.native_recover().map_err(error)?;
        let (sender, mut receiver) = mpsc::unbounded_channel::<Pending>();
        let (cancel, mut cancelled) = watch::channel(false);
        let state = Arc::new(Mutex::new(State {
            sender: Some(sender),
            fault: None,
            aborted: false,
            mode: Mode::None,
            pending: 0,
            #[cfg(feature = "testing")]
            commit_pause: None,
        }));
        let changed = Arc::new(Notify::new());
        let worker_state = state.clone();
        let worker_store = store.clone();
        let worker_changed = changed.clone();
        let worker_report = report.clone();
        let worker = tokio::spawn(async move {
            let mut failure = None;
            while let Some(mut pending) = receiver.recv().await {
                let replicated = fault_error(&worker_state.lock().unwrap()).map_or(Ok(()), Err);
                let replicated = match replicated {
                    Err(error) => Err(error),
                    Ok(()) => tokio::select! {
                        biased;
                        _ = cancelled.changed() => Err(stored_error(true, "KV service aborted; admitted outcome is unknown".into())),
                        result = &mut pending.assignment.completion => result,
                    },
                };
                #[cfg(feature = "testing")]
                if replicated.is_ok() {
                    let pause = worker_state.lock().unwrap().commit_pause.take();
                    if let Some(pause) = pause {
                        let _ = pause.entered.send(());
                        tokio::select! {
                            biased;
                            _ = cancelled.changed() => {},
                            _ = pause.release => {},
                        }
                    }
                }
                let mut state = worker_state.lock().unwrap();
                let result = replicated.and_then(|()| {
                    if let Some(error) = fault_error(&state) {
                        return Err(error);
                    }
                    worker_store
                        .native_commit(pending.assignment.lsn)
                        .map_err(error)?;
                    Ok(pending.assignment.lsn)
                });
                let result = if result.as_ref().err().is_some_and(is_abort) && !state.aborted {
                    fault_error(&state).map_or(result, Err)
                } else {
                    result
                };
                state.pending -= 1;
                match result {
                    Ok(lsn) => {
                        let _ = pending.reply.send(Ok(lsn));
                        drop(state);
                        worker_changed.notify_waiters();
                    }
                    Err(error) => {
                        let aborted = is_abort(&error);
                        let reason = format!(
                            "KV admitted write {} failed: {error}",
                            pending.assignment.lsn
                        );
                        failure.get_or_insert_with(|| (aborted, reason.clone()));
                        let first = record_fault(&mut state, &reason);
                        if first && aborted {
                            state.aborted = true;
                        }
                        drop(state);
                        if aborted {
                            worker_changed.notify_waiters();
                        } else {
                            publish_fault(&worker_changed, worker_report.as_ref(), reason, first);
                        }
                        let _ = pending.reply.send(Err(error));
                    }
                }
            }
            match failure {
                Some((aborted, reason)) => Err(stored_error(aborted, reason)),
                None => Ok(()),
            }
        });
        Ok(Self {
            store,
            state,
            changed,
            cancel,
            drain: AsyncMutex::new(Drain {
                worker: Some(worker),
                result: None,
            }),
            report,
        })
    }

    pub(crate) async fn wait_for_role_drain(&self) -> io::Result<()> {
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            {
                let state = self.state.lock().unwrap();
                live(&state)?;
                if state.pending == 0 && !matches!(state.mode, Mode::Copy { .. }) {
                    return Ok(());
                }
            }
            changed.await;
        }
    }

    pub fn promote(&self) -> io::Result<()> {
        let mut state = self.state.lock().unwrap();
        live(&state)?;
        if state.pending != 0 || matches!(state.mode, Mode::Copy { .. }) {
            return Err(io::Error::other(
                "KV promotion before local drainage/copy EOF",
            ));
        }
        state.mode = Mode::Primary;
        Ok(())
    }

    pub fn demote(&self) -> io::Result<()> {
        let mut state = self.state.lock().unwrap();
        live(&state)?;
        if state.pending != 0 || state.mode != Mode::Primary {
            return Err(io::Error::other(
                "KV demotion before local drainage/primary",
            ));
        }
        state.mode = Mode::Secondary;
        self.changed.notify_waiters();
        Ok(())
    }

    pub fn begin_put(
        &self,
        key: String,
        value: String,
        admit: impl FnOnce(Bytes) -> io::Result<Assignment>,
    ) -> io::Result<Write> {
        let data = KvPersistence::encode_put(key, value).map_err(error)?;
        let mut state = self.state.lock().unwrap();
        live(&state)?;
        if state.mode != Mode::Primary {
            return Err(io::Error::other("KV service is not primary"));
        }
        let assignment = admit(data.clone())?;
        let lsn = assignment.lsn;
        if let Err(error) = self.store.native_prepare(lsn, data, false) {
            let reason =
                format!("KV prepare after assignment {lsn} failed; outcome unknown: {error}");
            let first = record_fault(&mut state, &reason);
            drop(state);
            publish_fault(&self.changed, self.report.as_ref(), reason.clone(), first);
            return Err(io::Error::other(reason));
        }
        let (reply, outcome) = oneshot::channel();
        if state
            .sender
            .as_ref()
            .unwrap()
            .send(Pending { assignment, reply })
            .is_err()
        {
            let reason = "KV worker stopped after assignment; outcome unknown".to_string();
            let first = record_fault(&mut state, &reason);
            drop(state);
            publish_fault(&self.changed, self.report.as_ref(), reason.clone(), first);
            return Err(io::Error::other(reason));
        }
        state.pending += 1;
        Ok(Write {
            lsn,
            outcome,
            result: None,
        })
    }

    pub fn get(
        &self,
        key: &str,
        check_access: impl FnOnce() -> io::Result<()>,
    ) -> io::Result<Option<String>> {
        check_access()?;
        let state = self.state.lock().unwrap();
        live(&state)?;
        self.store.native_get(key).map_err(error)
    }

    pub fn progress(&self) -> io::Result<i64> {
        let state = self.state.lock().unwrap();
        live(&state)?;
        Ok(self.store.native_progress())
    }

    pub fn update_epoch(&self, data_loss: i64, configuration: i64) -> io::Result<()> {
        let state = self.state.lock().unwrap();
        live(&state)?;
        self.store
            .update_epoch(kuberic_runtime::protocol::types::Epoch {
                data_loss_number: data_loss,
                configuration_number: configuration,
            })
            .map_err(error)
    }

    pub async fn snapshot(&self, lsn: i64) -> io::Result<Bytes> {
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            {
                let state = self.state.lock().unwrap();
                live(&state)?;
                if self.store.native_progress() >= lsn {
                    return self.store.native_snapshot(lsn).map_err(error);
                }
            }
            changed.await;
        }
    }

    pub(crate) fn restore_retained_secondary(&self, native_boundary: i64) -> io::Result<()> {
        let mut state = self.state.lock().unwrap();
        live(&state)?;
        if native_boundary < 0
            || state.pending != 0
            || state.mode != Mode::None
            || self.store.native_progress() != native_boundary
        {
            return Err(io::Error::other(
                "KV retained initialization differs from native checkpoint",
            ));
        }
        state.mode = Mode::Secondary;
        self.changed.notify_waiters();
        Ok(())
    }

    pub fn begin_copy(&self) -> io::Result<()> {
        let mut state = self.state.lock().unwrap();
        live(&state)?;
        if state.pending != 0 || !matches!(state.mode, Mode::None | Mode::Secondary) {
            return Err(io::Error::other("KV copy requires a drained non-primary"));
        }
        state.mode = Mode::Copy { installed: false };
        Ok(())
    }

    pub fn install_copy(&self, data: Bytes) -> io::Result<()> {
        let mut state = self.state.lock().unwrap();
        live(&state)?;
        if state.mode != (Mode::Copy { installed: false }) {
            return Err(io::Error::other("KV snapshot outside its copy lifetime"));
        }
        let result = self.store.native_install(data).map_err(error);
        if let Err(error) = result {
            let reason = error.to_string();
            let first = record_fault(&mut state, &reason);
            drop(state);
            publish_fault(&self.changed, self.report.as_ref(), reason, first);
            return Err(error);
        }
        state.mode = Mode::Copy { installed: true };
        self.changed.notify_waiters();
        Ok(())
    }

    pub fn finish_copy(&self) -> io::Result<()> {
        let mut state = self.state.lock().unwrap();
        live(&state)?;
        if state.mode != (Mode::Copy { installed: true }) {
            return Err(io::Error::other("KV copy EOF before durable snapshot"));
        }
        state.mode = Mode::Secondary;
        self.changed.notify_waiters();
        Ok(())
    }

    pub fn apply_secondary(&self, lsn: i64, data: Bytes) -> io::Result<()> {
        let mut state = self.state.lock().unwrap();
        live(&state)?;
        if state.mode != Mode::Secondary {
            return Err(io::Error::other("KV replication before copy EOF/secondary"));
        }
        let result = self.store.native_prepare(lsn, data, true).map_err(error);
        let first = result
            .as_ref()
            .err()
            .is_some_and(|error| record_fault(&mut state, &error.to_string()));
        drop(state);
        if let Err(error) = &result {
            publish_fault(
                &self.changed,
                self.report.as_ref(),
                error.to_string(),
                first,
            );
        }
        self.changed.notify_waiters();
        result
    }

    pub fn abort(&self) {
        let reason = "KV service aborted; admitted outcomes are unknown".to_owned();
        {
            let mut state = self.state.lock().unwrap();
            let first = record_fault(&mut state, &reason);
            if first {
                state.aborted = true;
            }
        }
        self.changed.notify_waiters();
        self.cancel.send_replace(true);
    }

    /// Call after native Close/Abort resolves admitted engine work. This is a
    /// separate, cancellation-safe observation of service-local drainage.
    pub async fn close(&self) -> io::Result<()> {
        self.state.lock().unwrap().sender.take();
        self.changed.notify_waiters();
        let mut drain = self.drain.lock().await;
        if drain.result.is_none() {
            let result = drain.worker.as_mut().unwrap().await;
            drain.worker.take();
            let fault = fault_error(&self.state.lock().unwrap())
                .map(|error| (is_abort(&error), error.to_string()));
            drain.result = Some(match result {
                Ok(Ok(())) => fault.map_or(Ok(()), Err),
                Ok(Err(error)) => Err((is_abort(&error), error.to_string())),
                Err(error) => Err((false, format!("KV worker failed: {error}"))),
            });
        }
        let result = drain
            .result
            .as_ref()
            .unwrap()
            .as_ref()
            .copied()
            .map_err(|(aborted, reason)| stored_error(*aborted, reason.clone()));
        if let Err(error) = &result {
            fence(
                &self.state,
                &self.changed,
                self.report.as_ref(),
                error.to_string(),
            );
        }
        result
    }

    #[cfg(feature = "testing")]
    pub fn pause_next_commit(&self) -> CommitGate {
        let (entered, observed) = oneshot::channel();
        let (release, resumed) = oneshot::channel();
        let mut state = self.state.lock().unwrap();
        assert!(state.commit_pause.is_none());
        state.commit_pause = Some(CommitPause {
            entered,
            release: resumed,
        });
        CommitGate {
            entered: observed,
            release,
        }
    }
}

impl Drop for Committer {
    fn drop(&mut self) {
        if self.drain.get_mut().result.is_none() {
            self.abort();
        }
    }
}

#[cfg(feature = "testing")]
struct CommitPause {
    entered: oneshot::Sender<()>,
    release: oneshot::Receiver<()>,
}

#[cfg(feature = "testing")]
pub struct CommitGate {
    entered: oneshot::Receiver<()>,
    release: oneshot::Sender<()>,
}

#[cfg(feature = "testing")]
impl CommitGate {
    pub async fn entered(&mut self) {
        (&mut self.entered)
            .await
            .expect("KV local commit gate entered");
    }

    pub fn release(self) {
        let _ = self.release.send(());
    }
}

#[cfg(all(test, feature = "testing"))]
mod tests {
    use super::*;
    use futures::FutureExt;

    fn open(path: &Path) -> (Committer, Arc<Mutex<Vec<String>>>) {
        let faults = Arc::new(Mutex::new(Vec::new()));
        let reported = faults.clone();
        let committer = Committer::open(
            path,
            Arc::new(move |reason| {
                reported.lock().unwrap().push(reason);
            }),
        )
        .unwrap();
        (committer, faults)
    }

    fn assigned(lsn: i64) -> Assignment {
        Assignment {
            lsn,
            completion: Box::pin(async { Ok(()) }),
        }
    }

    #[tokio::test]
    async fn retained_secondary_requires_exact_native_boundary_and_preserves_data_without_copy() {
        let root = tempfile::tempdir().unwrap();
        let (source, _) = open(root.path());
        source.promote().unwrap();
        source
            .begin_put("retained".into(), "acknowledged".into(), |_| {
                Ok(assigned(1))
            })
            .unwrap()
            .wait()
            .await
            .unwrap();
        source.close().await.unwrap();
        drop(source);
        let (restored, faults) = open(root.path());
        assert!(restored.apply_secondary(2, Bytes::new()).is_err());
        assert!(restored.restore_retained_secondary(-1).is_err());
        assert!(restored.restore_retained_secondary(0).is_err());
        assert!(restored.restore_retained_secondary(2).is_err());
        assert_eq!(restored.progress().unwrap(), 1);
        assert_eq!(
            restored.get("retained", || Ok(())).unwrap().as_deref(),
            Some("acknowledged")
        );
        restored.restore_retained_secondary(1).unwrap();
        assert!(restored.restore_retained_secondary(1).is_err());
        restored
            .apply_secondary(
                2,
                KvPersistence::encode_put("later".into(), "replicated".into()).unwrap(),
            )
            .unwrap();
        assert_eq!(restored.progress().unwrap(), 2);
        assert_eq!(
            restored.get("retained", || Ok(())).unwrap().as_deref(),
            Some("acknowledged")
        );
        assert_eq!(
            restored.get("later", || Ok(())).unwrap().as_deref(),
            Some("replicated")
        );
        assert!(faults.lock().unwrap().is_empty());
        restored.close().await.unwrap();
        assert!(restored.restore_retained_secondary(2).is_err());
        let copy = tempfile::tempdir().unwrap();
        let (copy, _) = open(copy.path());
        copy.begin_copy().unwrap();
        assert!(copy.restore_retained_secondary(0).is_err());
        assert!(copy.finish_copy().is_err());
        copy.close().await.unwrap();
    }

    #[tokio::test]
    async fn native_cancellation_before_service_abort_is_typed_and_not_a_new_fault() {
        for cancelled in [true, false] {
            let directory = tempfile::tempdir().unwrap();
            let (kv, faults) = open(directory.path());
            kv.promote().unwrap();
            let cause = if cancelled {
                io::Error::other(kuberic_runtime::RuntimeError::OperationCancelled)
            } else {
                io::Error::new(io::ErrorKind::Interrupted, "ordinary worker failure")
            };
            let mut write = kv
                .begin_put("k".into(), "uncommitted".into(), |_| {
                    Ok(Assignment {
                        lsn: 1,
                        completion: Box::pin(async move { Err(cause) }),
                    })
                })
                .unwrap();
            assert_eq!(is_abort(&write.wait().await.unwrap_err()), cancelled);
            assert_eq!(is_abort(&write.wait().await.unwrap_err()), cancelled);
            assert_eq!(kv.store.native_progress(), 0);
            assert_eq!(faults.lock().unwrap().is_empty(), cancelled);
            assert_eq!(is_abort(&kv.close().await.unwrap_err()), cancelled);
        }
    }

    #[tokio::test]
    async fn prior_fault_is_not_hidden_by_later_native_cancellation() {
        let directory = tempfile::tempdir().unwrap();
        let (kv, faults) = open(directory.path());
        kv.promote().unwrap();
        let entered = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let (started, released) = (entered.clone(), release.clone());
        let mut write = kv
            .begin_put("k".into(), "uncommitted".into(), |_| {
                Ok(Assignment {
                    lsn: 1,
                    completion: Box::pin(async move {
                        started.notify_one();
                        released.notified().await;
                        Err(io::Error::other(
                            kuberic_runtime::RuntimeError::OperationCancelled,
                        ))
                    }),
                })
            })
            .unwrap();
        entered.notified().await;
        fence(
            &kv.state,
            &kv.changed,
            kv.report.as_ref(),
            "prior application failure".into(),
        );
        release.notify_one();
        let error = write.wait().await.unwrap_err();
        assert!(!is_abort(&error));
        assert!(error.to_string().contains("prior application failure"));
        assert!(!is_abort(&kv.close().await.unwrap_err()));
        assert_eq!(faults.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn out_of_order_completions_and_cancelled_waiters_preserve_fifo_commit() {
        let directory = tempfile::tempdir().unwrap();
        let (kv, faults) = open(directory.path());
        kv.promote().unwrap();
        let (complete, completed) = oneshot::channel();
        let mut first = kv
            .begin_put("k".into(), "a".into(), |_| {
                Ok(Assignment {
                    lsn: 1,
                    completion: Box::pin(async move { completed.await.map_err(error) }),
                })
            })
            .unwrap();
        let mut second = kv
            .begin_put("k".into(), "b".into(), |_| Ok(assigned(2)))
            .unwrap();
        assert!(first.wait().now_or_never().is_none());
        assert!(second.wait().now_or_never().is_none());
        assert_eq!(kv.progress().unwrap(), 0);
        assert_eq!(kv.get("k", || Ok(())).unwrap(), None);
        complete.send(()).unwrap();
        assert_eq!(first.wait().await.unwrap(), 1);
        assert_eq!(second.wait().await.unwrap(), 2);
        assert_eq!(second.wait().await.unwrap(), 2);
        assert_eq!(kv.get("k", || Ok(())).unwrap().as_deref(), Some("b"));
        assert!(
            kv.get("k", || Err(io::Error::other("authority fenced")))
                .is_err()
        );
        assert!(faults.lock().unwrap().is_empty());
        kv.close().await.unwrap();
    }

    #[tokio::test]
    async fn admission_failure_is_not_an_assigned_write_and_post_assignment_failure_is_ambiguous() {
        let directory = tempfile::tempdir().unwrap();
        let (kv, faults) = open(directory.path());
        kv.promote().unwrap();
        assert!(
            kv.begin_put("k".into(), "a".into(), |_| Err(io::Error::other(
                "NotPrimary"
            )))
            .is_err()
        );
        assert_eq!(kv.progress().unwrap(), 0);
        assert!(faults.lock().unwrap().is_empty());
        let mut first = kv
            .begin_put("k".into(), "a".into(), |_| Ok(assigned(1)))
            .unwrap();
        first.wait().await.unwrap();
        kv.store.fail_next_persist_for_test();
        let failure = kv
            .begin_put("k".into(), "b".into(), |_| Ok(assigned(2)))
            .err()
            .unwrap()
            .to_string();
        assert!(failure.contains("outcome unknown"));
        assert!(kv.get("k", || Ok(())).is_err());
        assert_eq!(faults.lock().unwrap().len(), 1);
        assert!(kv.close().await.is_err());
        drop(kv);
        let (reopened, _) = open(directory.path());
        assert_eq!(reopened.progress().unwrap(), 1);
        assert_eq!(reopened.get("k", || Ok(())).unwrap().as_deref(), Some("a"));
        reopened.close().await.unwrap();
    }

    #[tokio::test]
    async fn commit_failure_stops_later_success_and_recovery_discards_only_uncommitted_prepares() {
        let directory = tempfile::tempdir().unwrap();
        let (kv, faults) = open(directory.path());
        kv.promote().unwrap();
        let mut first = kv
            .begin_put("k".into(), "a".into(), |_| Ok(assigned(1)))
            .unwrap();
        first.wait().await.unwrap();
        let mut gate = kv.pause_next_commit();
        let mut second = kv
            .begin_put("k".into(), "b".into(), |_| Ok(assigned(2)))
            .unwrap();
        let mut third = kv
            .begin_put("k".into(), "c".into(), |_| Ok(assigned(3)))
            .unwrap();
        gate.entered().await;
        kv.store.fail_next_persist_for_test();
        gate.release();
        assert!(second.wait().await.is_err());
        assert!(third.wait().await.is_err());
        assert_eq!(faults.lock().unwrap().len(), 1);
        assert!(kv.close().await.is_err());
        drop(kv);
        let (reopened, _) = open(directory.path());
        assert_eq!(reopened.progress().unwrap(), 1);
        assert_eq!(reopened.get("k", || Ok(())).unwrap().as_deref(), Some("a"));
        reopened.promote().unwrap();
        assert_eq!(
            reopened
                .begin_put("k".into(), "d".into(), |_| Ok(assigned(2)))
                .unwrap()
                .wait()
                .await
                .unwrap(),
            2
        );
        reopened.close().await.unwrap();
    }

    #[tokio::test]
    async fn copy_requires_durable_complete_snapshot_and_rejects_conflicting_or_regressing_state() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        let (source, _) = open(a.path());
        source.promote().unwrap();
        source
            .begin_put("k".into(), "a".into(), |_| Ok(assigned(1)))
            .unwrap()
            .wait()
            .await
            .unwrap();
        let snapshot = source.snapshot(1).await.unwrap();
        let (target, faults) = open(b.path());
        target.begin_copy().unwrap();
        assert!(target.finish_copy().is_err());
        assert!(target.promote().is_err());
        assert!(target.apply_secondary(1, Bytes::new()).is_err());
        target.install_copy(snapshot.clone()).unwrap();
        assert!(target.install_copy(snapshot).is_err());
        target.finish_copy().unwrap();
        target
            .apply_secondary(
                2,
                KvPersistence::encode_put("k".into(), "b".into()).unwrap(),
            )
            .unwrap();
        assert_eq!(target.progress().unwrap(), 2);
        target.begin_copy().unwrap();
        assert!(
            target
                .install_copy(source.snapshot(1).await.unwrap())
                .is_err()
        );
        assert_eq!(faults.lock().unwrap().len(), 1);
        assert!(target.close().await.is_err());
        let conflict_directory = tempfile::tempdir().unwrap();
        let (conflict, faults) = open(conflict_directory.path());
        conflict.begin_copy().unwrap();
        conflict
            .install_copy(source.snapshot(1).await.unwrap())
            .unwrap();
        conflict.finish_copy().unwrap();
        conflict.begin_copy().unwrap();
        let mut changed: serde_json::Value =
            serde_json::from_slice(&source.snapshot(1).await.unwrap()).unwrap();
        changed["values"]["k"] = "conflict".into();
        assert!(
            conflict
                .install_copy(Bytes::from(serde_json::to_vec(&changed).unwrap()))
                .is_err()
        );
        assert_eq!(faults.lock().unwrap().len(), 1);
        assert!(conflict.close().await.is_err());
        source.close().await.unwrap();
        let c = tempfile::tempdir().unwrap();
        let (corrupt, faults) = open(c.path());
        corrupt.begin_copy().unwrap();
        assert!(corrupt.install_copy(Bytes::from_static(b"{}")).is_err());
        assert_eq!(faults.lock().unwrap().len(), 1);
        assert!(corrupt.close().await.is_err());
    }

    #[tokio::test]
    async fn corrupt_recovery_and_worker_panic_do_not_become_serving_or_cached_success() {
        let directory = tempfile::tempdir().unwrap();
        let (kv, faults) = open(directory.path());
        kv.promote().unwrap();
        let mut write = kv
            .begin_put("k".into(), "a".into(), |_| {
                Ok(Assignment {
                    lsn: 1,
                    completion: Box::pin(async { panic!("completion worker panic") }),
                })
            })
            .unwrap();
        assert!(write.wait().await.is_err());
        assert!(write.wait().await.is_err());
        assert!(kv.get("k", || Ok(())).is_err());
        assert!(
            kv.close()
                .await
                .unwrap_err()
                .to_string()
                .contains("KV worker failed")
        );
        assert_eq!(faults.lock().unwrap().len(), 1);
        drop(kv);
        let path = directory.path().join("state.json");
        let mut state: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        state["applied_lsn"] = 5.into();
        std::fs::write(&path, serde_json::to_vec(&state).unwrap()).unwrap();
        assert!(Committer::open(directory.path(), Arc::new(|_| {})).is_err());
        let unreadable = tempfile::tempdir().unwrap();
        std::fs::create_dir(unreadable.path().join("state.json")).unwrap();
        assert!(Committer::open(unreadable.path(), Arc::new(|_| {})).is_err());
    }
}
