// Copyright (c) Microsoft Corporation. All rights reserved.
// Licensed under the MIT License. See LICENSES/service-fabric-MIT.txt.
//
// Service-owned ordering from Store/ReplicatedStore.TransactionReplicator.cpp:
// 606-796,848-900, microsoft/service-fabric@3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d.

//! Private SQLite application adaptation, not a replicator or selected factory.
//! A retained SQL worker owns the synchronous VFS transaction beyond client
//! cancellation. The barrier awaits native quorum, commits the frame log, then
//! releases WAL publication. Only successful SQLite publication completes a
//! client write. New-instance recovery requires retirement of the prior owner.

use std::future::Future;
use std::io;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};

use bytes::Bytes;
use tokio::sync::{Mutex as AsyncMutex, Notify, mpsc, oneshot, watch};
use tokio::task::JoinHandle;

use crate::SqlitePersistence;
use crate::commit_barrier::{BarrierError, CommitBarrier, Transaction};
use crate::connection::SqliteConnection;
use crate::connection::Statement;
use crate::frames::{WalFrameSet, frames_from_wal_bytes};

pub struct Assignment {
    pub lsn: i64,
    pub completion: Pin<Box<dyn Future<Output = io::Result<()>> + Send>>,
}

pub type Admission = Arc<dyn Fn(Bytes) -> io::Result<Assignment> + Send + Sync>;
pub type Access = Arc<dyn Fn(bool) -> io::Result<()> + Send + Sync>;

pub struct Write {
    assigned: watch::Receiver<Option<i64>>,
    outcome: oneshot::Receiver<io::Result<Execution>>,
    result: Option<Result<Execution, (bool, String)>>,
}

#[derive(Clone, Debug)]
pub struct Execution {
    pub rows: Vec<usize>,
    pub rowid: i64,
    pub lsn: i64,
}

impl Write {
    pub(crate) fn admitted_lsn(&self) -> Option<i64> {
        *self.assigned.borrow()
    }

    pub async fn assigned(&mut self) -> io::Result<i64> {
        loop {
            if let Some(lsn) = *self.assigned.borrow() {
                return Ok(lsn);
            }
            self.assigned
                .changed()
                .await
                .map_err(|_| io::Error::other("SQL ended before assignment"))?;
        }
    }

    pub async fn wait(&mut self) -> io::Result<i64> {
        self.wait_execution().await.map(|execution| execution.lsn)
    }

    pub async fn wait_execution(&mut self) -> io::Result<Execution> {
        if self.result.is_none() {
            self.result = Some(match (&mut self.outcome).await {
                Ok(result) => result.map_err(|error| (is_abort(&error), error.to_string())),
                Err(_) => Err((false, "SQLite worker abandoned client outcome".into())),
            });
        }
        self.result
            .as_ref()
            .unwrap()
            .as_ref()
            .cloned()
            .map_err(|(aborted, reason)| stored_error(*aborted, reason.clone()))
    }
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

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    None,
    Primary,
    Copy { installed: bool },
    Secondary,
}

struct Request {
    statements: Vec<Statement>,
    assigned: watch::Sender<Option<i64>>,
    reply: oneshot::Sender<io::Result<Execution>>,
}

struct State {
    sender: Option<mpsc::UnboundedSender<Request>>,
    fault: Option<String>,
    aborted: bool,
    mode: Mode,
    admission: Option<Admission>,
    pending: usize,
    assignment: Option<watch::Sender<Option<i64>>>,
    lsn: Option<i64>,
    failure: Option<String>,
    failure_aborted: bool,
}

struct Core {
    store: Arc<SqlitePersistence>,
    state: Mutex<State>,
    sql: Mutex<SqliteConnection>,
    changed: Notify,
    cancel: watch::Sender<bool>,
    runtime: tokio::runtime::Handle,
    vfs: String,
    report: Arc<dyn Fn(String) + Send + Sync>,
    access: Access,
    #[cfg(feature = "testing")]
    before_commit: crate::testing::PauseGate,
    #[cfg(feature = "testing")]
    before_publication: crate::testing::PauseGate,
    #[cfg(feature = "testing")]
    before_success: crate::testing::PauseGate,
    #[cfg(feature = "testing")]
    worker_exit: crate::testing::PauseGate,
    #[cfg(feature = "testing")]
    fail: std::sync::atomic::AtomicU8,
}

impl Core {
    fn healthy(state: &State) -> io::Result<()> {
        state.fault.as_ref().map_or(Ok(()), |fault| {
            Err(stored_error(state.aborted, fault.clone()))
        })
    }

    fn live(state: &State) -> io::Result<()> {
        Self::healthy(state)?;
        if state
            .sender
            .as_ref()
            .is_none_or(|sender| sender.is_closed())
        {
            return Err(io::Error::other("SQLite service is closing"));
        }
        Ok(())
    }

    fn record_fault(state: &mut State, reason: &str) -> bool {
        let first = state.fault.is_none();
        state.fault.get_or_insert_with(|| reason.to_owned());
        state.sender.take();
        state.admission.take();
        first
    }

    fn publish_fault(&self, reason: String, first: bool) {
        self.changed.notify_waiters();
        if first
            && std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| (self.report)(reason)))
                .is_err()
        {
            let reason = "SQLite fault callback panicked";
            tracing::error!(reason);
            let mut state = self.state.lock().unwrap();
            state.aborted = false;
            state
                .fault
                .as_mut()
                .unwrap()
                .push_str("; fault callback panicked");
        }
    }

    fn fault(&self, reason: String) {
        let first = Self::record_fault(&mut self.state.lock().unwrap(), &reason);
        self.publish_fault(reason, first);
    }

    fn execute(&self, statements: Vec<Statement>, vfs: &str) -> io::Result<Execution> {
        (self.access)(true)?;
        let mut sql = self.sql.lock().unwrap();
        {
            let state = self.state.lock().unwrap();
            Self::healthy(&state)?;
        }
        if !sql.is_open() {
            let path = self.store.materialize_committed()?;
            sql.open(&path, vfs, self.store.progress()?.committed_lsn)
                .map_err(error)?;
        }
        let executed = sql.execute(&statements).map_err(error);
        let lsn = self.state.lock().unwrap().lsn;
        match executed {
            Ok((rows, rowid)) => {
                if let Some(lsn) = lsn {
                    self.store.confirm_publication(lsn)?;
                    sql.visible_lsn = lsn;
                }
                Ok(Execution {
                    rows,
                    rowid,
                    lsn: self.store.progress()?.committed_lsn,
                })
            }
            Err(sql_error) => {
                let state = self.state.lock().unwrap();
                if let Some(fault) = state.fault.as_ref().filter(|_| !state.aborted) {
                    return Err(io::Error::other(fault.clone()));
                }
                Err(state.failure.as_ref().map_or(sql_error, |reason| {
                    stored_error(state.failure_aborted, reason.clone())
                }))
            }
        }
    }

    #[cfg(feature = "testing")]
    fn cut(&self, cut: FailureCut) -> io::Result<()> {
        if self
            .fail
            .compare_exchange(cut as u8, 0, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
        {
            return Err(io::Error::other("injected SQLite service cut"));
        }
        Ok(())
    }
}

struct Barrier(Weak<Core>);

impl Barrier {
    fn publish_transaction(&self, transaction: &Transaction<'_>) -> Result<(), BarrierError> {
        let core = self
            .0
            .upgrade()
            .ok_or_else(|| BarrierError::new("SQLite service retired"))?;
        let result = (|| {
            (core.access)(true)?;
            let frames = frames_from_wal_bytes(
                transaction.wal_offset,
                transaction.frames,
                transaction.page_size,
            );
            let frames = WalFrameSet {
                checksum: WalFrameSet::compute_checksum(&frames),
                frames,
                db_size_pages: transaction.database_pages,
            };
            frames.page_size()?;
            let data = Bytes::from(serde_json::to_vec(&frames).map_err(error)?);
            let mut cancelled = core.cancel.subscribe();
            let assignment = {
                let mut state = core.state.lock().unwrap();
                Core::healthy(&state)?;
                let admit = state
                    .admission
                    .as_ref()
                    .ok_or_else(|| io::Error::other("SQLite is not primary"))?;
                let assignment = admit(data.clone())?;
                state.lsn = Some(assignment.lsn);
                state
                    .assignment
                    .as_ref()
                    .unwrap()
                    .send_replace(Some(assignment.lsn));
                #[cfg(feature = "testing")]
                core.cut(FailureCut::Prepare)?;
                core.store.native_prepare(assignment.lsn, data, false)?;
                core.store.require_reconciliation(
                    "native SQL awaiting durable commit/publication".into(),
                )?;
                assignment
            };
            core.runtime.block_on(async {
                tokio::select! {
                    biased;
                    _ = cancelled.changed() => return Err(stored_error(true, "SQLite aborted; admitted outcome unknown".into())),
                    result = assignment.completion => result?,
                }
                #[cfg(feature = "testing")]
                tokio::select! {
                    biased;
                    _ = cancelled.changed() => return Err(stored_error(true, "SQLite aborted before local commit".into())),
                    _ = core.before_commit.pause() => {},
                }
                {
                    let state = core.state.lock().unwrap();
                    Core::healthy(&state)?;
                    #[cfg(feature = "testing")]
                    core.cut(FailureCut::Commit)?;
                    core.store.native_commit(assignment.lsn)?;
                }
                core.changed.notify_waiters();
                #[cfg(feature = "testing")]
                tokio::select! {
                    biased;
                    _ = cancelled.changed() => return Err(stored_error(true, "SQLite aborted before publication".into())),
                    _ = core.before_publication.pause() => {},
                }
                let state = core.state.lock().unwrap();
                Core::healthy(&state)?;
                #[cfg(feature = "testing")]
                core.cut(FailureCut::Publication)?;
                Ok(())
            })
        })();
        if let Err(error) = &result {
            let mut state = core.state.lock().unwrap();
            state.failure = Some(error.to_string());
            state.failure_aborted = is_abort(error);
            if state.lsn.is_some() {
                let reason =
                    format!("SQLite admitted publication failed; outcome unknown: {error}");
                let first = Core::record_fault(&mut state, &reason);
                if first && is_abort(error) {
                    state.aborted = true;
                }
                drop(state);
                if is_abort(error) {
                    core.changed.notify_waiters();
                } else {
                    core.publish_fault(reason, first);
                }
            }
        }
        result.map_err(|e| BarrierError::new(e.to_string()))
    }
}

impl CommitBarrier for Barrier {
    fn publish(&self, transaction: &Transaction<'_>) -> Result<(), BarrierError> {
        // Application callbacks/futures must not unwind across SQLite's C ABI.
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.publish_transaction(transaction)
        })) {
            Ok(result) => result,
            Err(_) => {
                let reason = "SQLite publication callback panicked; admitted outcome unknown";
                if let Some(core) = self.0.upgrade() {
                    core.state.clear_poison();
                    core.fault(reason.into());
                }
                Err(BarrierError::new(reason))
            }
        }
    }

    fn abandon(&self, reason: &str) {
        if let Some(core) = self.0.upgrade() {
            core.fault(format!("SQLite local WAL publication failed: {reason}"));
        }
    }
}

struct Drain {
    worker: Option<JoinHandle<io::Result<()>>>,
    result: Option<Result<(), (bool, String)>>,
}

pub struct Committer {
    core: Arc<Core>,
    drain: AsyncMutex<Drain>,
}

impl Committer {
    pub fn open(
        root: PathBuf,
        access: Access,
        report: Arc<dyn Fn(String) + Send + Sync>,
    ) -> io::Result<Self> {
        let store = SqlitePersistence::open(root)?;
        Self::with_store(Arc::new(store), access, report)
    }

    pub(crate) fn with_store(
        store: Arc<SqlitePersistence>,
        access: Access,
        report: Arc<dyn Fn(String) + Send + Sync>,
    ) -> io::Result<Self> {
        store.initialize()?;
        store.native_recover()?;
        let (sender, mut receiver) = mpsc::unbounded_channel::<Request>();
        let (cancel, _) = watch::channel(false);
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let vfs = format!(
            "kuberic-private-v1-{}",
            NEXT.fetch_add(1, Ordering::Relaxed)
        );
        let core = Arc::new(Core {
            store,
            state: Mutex::new(State {
                sender: Some(sender),
                fault: None,
                aborted: false,
                mode: Mode::None,
                admission: None,
                pending: 0,
                assignment: None,
                lsn: None,
                failure: None,
                failure_aborted: false,
            }),
            sql: Mutex::new(SqliteConnection::default()),
            changed: Notify::new(),
            cancel,
            runtime: tokio::runtime::Handle::current(),
            vfs: vfs.clone(),
            report,
            access,
            #[cfg(feature = "testing")]
            before_commit: Default::default(),
            #[cfg(feature = "testing")]
            before_publication: Default::default(),
            #[cfg(feature = "testing")]
            before_success: Default::default(),
            #[cfg(feature = "testing")]
            worker_exit: Default::default(),
            #[cfg(feature = "testing")]
            fail: Default::default(),
        });
        crate::commit_barrier::register(&vfs, Arc::new(Barrier(Arc::downgrade(&core))))
            .map_err(error)?;
        let worker_core = core.clone();
        let worker = tokio::spawn(async move {
            let core = worker_core;
            let mut failed = None;
            while let Some(request) = receiver.recv().await {
                {
                    let mut state = core.state.lock().unwrap();
                    state.assignment = Some(request.assigned);
                    state.lsn = None;
                    state.failure = None;
                    state.failure_aborted = false;
                }
                let sql_core = core.clone();
                let sql_vfs = vfs.clone();
                let result = tokio::task::spawn_blocking(move || {
                    let result = sql_core.execute(request.statements, &sql_vfs);
                    #[cfg(feature = "testing")]
                    sql_core.runtime.block_on(sql_core.worker_exit.pause());
                    result
                })
                .await;
                let result = match result {
                    Ok(result) => result,
                    Err(error) => {
                        core.fault(format!("SQLite worker failed: {error}"));
                        Err(crate::v1::error(error))
                    }
                };
                #[cfg(feature = "testing")]
                {
                    let mut cancelled = core.cancel.subscribe();
                    if !*cancelled.borrow() {
                        tokio::select! {
                            biased;
                            _ = cancelled.changed() => {},
                            _ = core.before_success.pause() => {},
                        }
                    }
                }
                let mut state = core.state.lock().unwrap();
                let result = result.and_then(|execution| {
                    Core::healthy(&state)?;
                    Ok(execution)
                });
                let uncertain = result.is_err() && state.lsn.is_some();
                state.pending -= 1;
                state.assignment.take();
                if uncertain {
                    let reason = format!(
                        "SQLite assigned write failed; outcome unknown: {}",
                        result.as_ref().unwrap_err()
                    );
                    let first = Core::record_fault(&mut state, &reason);
                    drop(state);
                    core.publish_fault(reason.clone(), first);
                    failed.get_or_insert((is_abort(result.as_ref().unwrap_err()), reason));
                } else {
                    let _ = request.reply.send(result);
                    drop(state);
                    core.changed.notify_waiters();
                    continue;
                }
                let _ = request.reply.send(result);
                core.changed.notify_waiters();
            }
            core.sql.lock().unwrap().close();
            failed.map_or(Ok(()), |(aborted, reason)| {
                Err(stored_error(aborted, reason))
            })
        });
        Ok(Self {
            core,
            drain: AsyncMutex::new(Drain {
                worker: Some(worker),
                result: None,
            }),
        })
    }

    pub(crate) async fn wait_for_role_drain(&self) -> io::Result<()> {
        loop {
            let changed = self.core.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            {
                let state = self.core.state.lock().unwrap();
                Core::live(&state)?;
                if state.pending == 0 && !matches!(state.mode, Mode::Copy { .. }) {
                    return Ok(());
                }
            }
            changed.await;
        }
    }

    pub fn promote(&self, admission: Admission) -> io::Result<()> {
        let mut state = self.core.state.lock().unwrap();
        Core::live(&state)?;
        if state.pending != 0 || matches!(state.mode, Mode::Copy { .. }) {
            return Err(io::Error::other(
                "SQLite promotion before drainage/copy EOF",
            ));
        }
        state.admission = Some(admission);
        state.mode = Mode::Primary;
        Ok(())
    }

    pub fn demote(&self) -> io::Result<()> {
        let mut state = self.core.state.lock().unwrap();
        Core::live(&state)?;
        if state.pending != 0 || state.mode != Mode::Primary {
            return Err(io::Error::other("SQLite demotion before primary drainage"));
        }
        state.admission.take();
        state.mode = Mode::Secondary;
        self.core.sql.lock().unwrap().close();
        Ok(())
    }

    pub fn begin_execute(&self, statements: Vec<String>) -> io::Result<Write> {
        self.begin_statements(
            statements
                .into_iter()
                .map(|sql| (sql, Vec::new()))
                .collect(),
        )
    }

    pub(crate) fn begin_statements(&self, statements: Vec<Statement>) -> io::Result<Write> {
        (self.core.access)(true)?;
        let mut state = self.core.state.lock().unwrap();
        Core::live(&state)?;
        if state.mode != Mode::Primary {
            return Err(io::Error::other("SQLite service is not primary"));
        }
        let (reply, outcome) = oneshot::channel();
        let (assigned, assignment) = watch::channel(None);
        state
            .sender
            .as_ref()
            .unwrap()
            .send(Request {
                statements,
                assigned,
                reply,
            })
            .map_err(|_| io::Error::other("SQLite worker stopped"))?;
        state.pending += 1;
        Ok(Write {
            assigned: assignment,
            outcome,
            result: None,
        })
    }

    pub fn query(&self, sql: &str) -> io::Result<Vec<Vec<rusqlite::types::Value>>> {
        self.query_with_params(sql, &[]).map(|(_, rows)| rows)
    }

    pub(crate) fn query_with_params(
        &self,
        sql: &str,
        params: &[rusqlite::types::Value],
    ) -> io::Result<(Vec<String>, Vec<Vec<rusqlite::types::Value>>)> {
        (self.core.access)(false)?;
        let state = self.core.state.lock().unwrap();
        Core::live(&state)?;
        if state.pending != 0 || matches!(state.mode, Mode::None | Mode::Copy { .. }) {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "SQLite transaction/copy is in progress",
            ));
        }
        let mut connection = self.core.sql.lock().unwrap();
        if !connection.is_open() {
            let path = self.core.store.materialize_committed()?;
            connection
                .open(
                    &path,
                    &self.core.vfs,
                    self.core.store.progress()?.committed_lsn,
                )
                .map_err(error)?;
        }
        connection.query(sql, params).map_err(error)
    }

    pub fn progress(&self) -> io::Result<i64> {
        Core::live(&self.core.state.lock().unwrap())?;
        Ok(self.core.store.progress()?.committed_lsn)
    }

    pub async fn snapshot(&self, lsn: i64) -> io::Result<Bytes> {
        loop {
            let changed = self.core.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            {
                let state = self.core.state.lock().unwrap();
                Core::live(&state)?;
                if self.core.store.progress()?.committed_lsn >= lsn {
                    return self.core.store.snapshot(lsn).map(Bytes::from);
                }
            }
            changed.await;
        }
    }

    pub(crate) fn restore_retained_secondary(&self, native_boundary: i64) -> io::Result<()> {
        let mut state = self.core.state.lock().unwrap();
        Core::live(&state)?;
        if native_boundary < 0
            || state.pending != 0
            || state.mode != Mode::None
            || self.core.store.progress()?.committed_lsn != native_boundary
        {
            return Err(io::Error::other(
                "SQLite retained initialization differs from native checkpoint",
            ));
        }
        state.mode = Mode::Secondary;
        self.core.changed.notify_waiters();
        Ok(())
    }

    pub fn begin_copy(&self) -> io::Result<()> {
        let mut state = self.core.state.lock().unwrap();
        Core::live(&state)?;
        if state.pending != 0 || !matches!(state.mode, Mode::None | Mode::Secondary) {
            return Err(io::Error::other("SQLite copy requires drained non-primary"));
        }
        self.core.sql.lock().unwrap().close();
        state.mode = Mode::Copy { installed: false };
        Ok(())
    }

    pub fn install_copy(&self, data: Bytes) -> io::Result<()> {
        let mut state = self.core.state.lock().unwrap();
        Core::live(&state)?;
        if state.mode != (Mode::Copy { installed: false }) {
            return Err(io::Error::other("SQLite snapshot outside copy lifetime"));
        }
        let result = self.core.store.native_install(&data);
        if result.is_ok() {
            state.mode = Mode::Copy { installed: true };
        }
        let first = result
            .as_ref()
            .err()
            .is_some_and(|error| Core::record_fault(&mut state, &error.to_string()));
        drop(state);
        if let Err(error) = &result {
            self.core.publish_fault(error.to_string(), first);
        }
        self.core.changed.notify_waiters();
        result
    }

    pub fn finish_copy(&self) -> io::Result<()> {
        let mut state = self.core.state.lock().unwrap();
        Core::live(&state)?;
        if state.mode != (Mode::Copy { installed: true }) {
            return Err(io::Error::other(
                "SQLite copy EOF before snapshot installation",
            ));
        }
        state.mode = Mode::Secondary;
        self.core.changed.notify_waiters();
        Ok(())
    }

    pub fn apply_secondary(&self, lsn: i64, data: Bytes) -> io::Result<()> {
        let mut state = self.core.state.lock().unwrap();
        Core::live(&state)?;
        if state.mode != Mode::Secondary {
            return Err(io::Error::other(
                "SQLite replication before secondary/copy EOF",
            ));
        }
        self.core.sql.lock().unwrap().close();
        let result = self.core.store.native_prepare(lsn, data, true);
        let first = result
            .as_ref()
            .err()
            .is_some_and(|error| Core::record_fault(&mut state, &error.to_string()));
        drop(state);
        if let Err(error) = &result {
            self.core.publish_fault(error.to_string(), first);
        }
        self.core.changed.notify_waiters();
        result
    }

    pub fn abort(&self) {
        let reason = "SQLite service aborted; admitted outcomes unknown".to_owned();
        {
            let mut state = self.core.state.lock().unwrap();
            let first = Core::record_fault(&mut state, &reason);
            if first {
                state.aborted = true;
            }
        }
        self.core.changed.notify_waiters();
        self.core.cancel.send_replace(true);
    }

    pub async fn close(&self) -> io::Result<()> {
        {
            let mut state = self.core.state.lock().unwrap();
            state.sender.take();
        }
        self.core.changed.notify_waiters();
        let mut drain = self.drain.lock().await;
        if drain.result.is_none() {
            let result = drain.worker.as_mut().unwrap().await;
            drain.worker.take();
            self.core.state.lock().unwrap().admission.take();
            drain.result = Some(match result {
                Ok(Ok(())) => {
                    let state = self.core.state.lock().unwrap();
                    state
                        .fault
                        .clone()
                        .map_or(Ok(()), |reason| Err((state.aborted, reason)))
                }
                Ok(Err(error)) => {
                    let state = self.core.state.lock().unwrap();
                    let mut diagnostic = error.to_string();
                    if let Some(fault) = &state.fault
                        && fault != &diagnostic
                    {
                        diagnostic.push_str("; ");
                        diagnostic.push_str(fault);
                    }
                    Err((is_abort(&error) && state.aborted, diagnostic))
                }
                Err(error) => Err((false, format!("SQLite worker failed: {error}"))),
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
            self.core.fault(error.to_string());
        }
        result
    }

    #[cfg(feature = "testing")]
    pub fn before_commit(&self) -> &crate::testing::PauseGate {
        &self.core.before_commit
    }
    #[cfg(feature = "testing")]
    pub fn before_publication(&self) -> &crate::testing::PauseGate {
        &self.core.before_publication
    }
    #[cfg(feature = "testing")]
    pub fn before_success(&self) -> &crate::testing::PauseGate {
        &self.core.before_success
    }
    #[cfg(feature = "testing")]
    pub fn worker_exit(&self) -> &crate::testing::PauseGate {
        &self.core.worker_exit
    }
    #[cfg(feature = "testing")]
    pub fn fail_once(&self, cut: FailureCut) {
        self.core.fail.store(cut as u8, Ordering::SeqCst);
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
#[derive(Clone, Copy)]
pub enum FailureCut {
    Prepare = 1,
    Commit = 2,
    Publication = 3,
}

#[cfg(all(test, feature = "testing"))]
mod tests;
