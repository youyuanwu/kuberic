use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use kuberic_runtime::replicator::TestingTcpRouteKind;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Mutex, mpsc, oneshot, watch};
use tokio::task::{JoinHandle, JoinSet};

const ACK_TIMEOUT: Duration = Duration::from_secs(5);

enum Command {
    Cut(bool, oneshot::Sender<()>),
    Stop,
}

struct Proxy {
    address: String,
    commands: mpsc::Sender<Command>,
    task: Option<JoinHandle<Result<()>>>,
    // Listener cancellation must not discard the only stream join owners.
    streams: Arc<Mutex<StreamOwners>>,
    stream_tasks: Arc<AtomicUsize>,
    failures: Vec<String>,
}

#[derive(Default)]
struct StreamOwners {
    tasks: JoinSet<()>,
    failures: Vec<String>,
    retained: Arc<AtomicUsize>,
}

async fn join_streams(streams: &mut StreamOwners, cancel: bool) -> Result<()> {
    if cancel {
        streams.tasks.abort_all();
    }
    while let Some(joined) = streams.tasks.join_next().await {
        streams
            .retained
            .store(streams.tasks.len(), Ordering::Release);
        if let Err(error) = joined
            && (!cancel || !error.is_cancelled())
        {
            streams.failures.push(error.to_string());
        }
    }
    ensure!(
        streams.failures.is_empty(),
        "forwarder failures: {}",
        streams.failures.join("; ")
    );
    Ok(())
}

impl Proxy {
    async fn new(backend: String, blocked: bool) -> Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let address = format!("http://{}", listener.local_addr()?);
        let (commands, mut receiver) = mpsc::channel(4);
        let stream_tasks = Arc::new(AtomicUsize::new(0));
        let streams = Arc::new(Mutex::new(StreamOwners {
            retained: stream_tasks.clone(),
            ..StreamOwners::default()
        }));
        let task_streams = streams.clone();
        let task = tokio::spawn(async move {
            let mut streams = task_streams.lock().await;
            let mut blocked = blocked;
            let result: Result<()> = async {
                loop {
                    tokio::select! {
                        biased;
                        command = receiver.recv() => match command {
                            Some(Command::Cut(cut, ack)) => {
                                if cut != blocked {
                                    blocked = cut;
                                    if cut {
                                        join_streams(&mut streams, true).await?;
                                    }
                                }
                                let _ = ack.send(());
                            }
                            Some(Command::Stop) | None => break,
                        },
                        accepted = listener.accept() => {
                            let (mut incoming, _) = accepted?;
                            if blocked { continue; }
                            let backend = backend.clone();
                            streams.tasks.spawn(async move {
                                if let Ok(mut outgoing) = TcpStream::connect(backend).await {
                                    // Resets and EOF are transport failures, not protocol replies.
                                    let _ = tokio::io::copy_bidirectional(&mut incoming, &mut outgoing).await;
                                }
                            });
                            streams.retained.store(streams.tasks.len(), Ordering::Release);
                        },
                        joined = streams.tasks.join_next(), if !streams.tasks.is_empty() => {
                            streams.retained.store(streams.tasks.len(), Ordering::Release);
                            if let Some(Err(error)) = joined { return Err(error.into()); }
                        }
                    }
                }
                Ok(())
            }.await;
            if let Err(error) = &result {
                streams.failures.push(format!("{error:#}"));
            }
            let drained = join_streams(&mut streams, true).await;
            match (result, drained) {
                (Err(error), Err(drain)) => {
                    Err(error.context(format!("stream cleanup: {drain:#}")))
                }
                (Err(error), _) | (_, Err(error)) => Err(error),
                _ => Ok(()),
            }
        });
        Ok(Self {
            address,
            commands,
            task: Some(task),
            streams,
            stream_tasks,
            failures: Vec::new(),
        })
    }

    async fn cut(&self, blocked: bool) -> Result<()> {
        let (ack, received) = oneshot::channel();
        tokio::time::timeout(ACK_TIMEOUT, async {
            self.commands.send(Command::Cut(blocked, ack)).await?;
            received.await?;
            Ok::<_, anyhow::Error>(())
        })
        .await??;
        Ok(())
    }

    async fn finish(&mut self) -> Result<()> {
        if let Some(task) = self.task.as_mut() {
            if let Err(error) =
                tokio::time::timeout(ACK_TIMEOUT, self.commands.send(Command::Stop)).await
            {
                self.failures.push(format!("proxy stop deadline: {error}"));
            }
            let result = match tokio::time::timeout(ACK_TIMEOUT, &mut *task).await {
                Ok(result) => result
                    .context("proxy task failed")
                    .and_then(|result| result),
                Err(error) => {
                    self.failures.push(format!("proxy join deadline: {error}"));
                    task.abort();
                    match task.await {
                        Err(join) if !join.is_cancelled() => Err(join.into()),
                        Ok(Err(failure)) => Err(failure),
                        _ => Ok(()),
                    }
                }
            };
            if let Err(error) = result {
                self.failures.push(format!("{error:#}"));
            }
            self.task = None;
        }
        if let Err(error) = join_streams(&mut *self.streams.lock().await, true).await {
            self.failures.push(format!("{error:#}"));
        }
        ensure!(
            self.failures.is_empty(),
            "proxy cleanup: {}",
            self.failures.join("; ")
        );
        Ok(())
    }
}

impl Drop for Proxy {
    fn drop(&mut self) {
        if let Some(task) = &self.task {
            task.abort();
        }
    }
}

#[derive(Debug, thiserror::Error)]
#[error("a transport fault already owns this network")]
pub(super) struct FaultAlreadyActive;

#[derive(Debug, thiserror::Error)]
#[error("invalid isolation token: {0}")]
pub(super) struct InvalidIsolation(&'static str);

#[derive(Clone)]
enum FaultStatus {
    Pending,
    Entered,
    Healed,
    Failed(String),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Pending,
    Entering,
    Entered,
    Restoring,
}

struct Fault {
    id: u64,
    members: BTreeSet<String>,
    phase: Phase,
    status: watch::Sender<FaultStatus>,
}

/// An exclusive owned transport cut, or an armed next-Build-dial cut.
///
/// Dropping requests restoration. Use the cluster's asynchronous healing or
/// finalization operation to acknowledge completion.
pub struct Isolation {
    network: Arc<Inner>,
    id: u64,
    status: watch::Receiver<FaultStatus>,
    released: bool,
    deadline: Option<tokio::time::Instant>,
}

impl Isolation {
    pub(super) fn with_deadline(mut self, deadline: tokio::time::Instant) -> Self {
        self.deadline = Some(self.deadline.map_or(deadline, |bound| bound.min(deadline)));
        self
    }

    /// Wait for existing affected streams to join and new connections to gate.
    ///
    /// A scenario deadline does not heal the fault or release its ownership.
    pub async fn entered(&mut self) -> Result<()> {
        if let Some(deadline) = self.deadline {
            tokio::time::timeout_at(deadline, self.wait_for_entry()).await?
        } else {
            self.wait_for_entry().await
        }
    }

    async fn wait_for_entry(&mut self) -> Result<()> {
        loop {
            if self
                .deadline
                .is_some_and(|deadline| tokio::time::Instant::now() >= deadline)
            {
                return Err(super::TestError::new(
                    super::ErrorKind::Deadline,
                    "transport entry deadline elapsed; fault ownership is retained",
                )
                .into());
            }
            let status = self.status.borrow_and_update().clone();
            match status {
                FaultStatus::Entered => return Ok(()),
                FaultStatus::Healed => anyhow::bail!("transport fault was healed before entry"),
                FaultStatus::Failed(error) => anyhow::bail!("transport fault failed: {error}"),
                FaultStatus::Pending => self.status.changed().await?,
            }
        }
    }
}

impl Drop for Isolation {
    fn drop(&mut self) {
        if !self.released && !matches!(*self.status.borrow(), FaultStatus::Healed) {
            self.network.restores.lock().unwrap().push(self.id);
        }
    }
}

#[derive(Default)]
struct State {
    owners: BTreeMap<String, String>,
    isolated: BTreeSet<String>,
    proxies: BTreeMap<(String, String), Proxy>,
    closed: bool,
    next_owner: u64,
    fault: Option<Fault>,
    fatal: Vec<String>,
}

#[derive(Default)]
struct Inner {
    state: Mutex<State>,
    restores: StdMutex<Vec<u64>>,
}

#[derive(Clone, Default)]
pub(super) struct Network(Arc<Inner>);

fn endpoint_key(endpoint: &str) -> String {
    endpoint.trim_end_matches('/').to_owned()
}

impl Network {
    pub(super) fn diagnostics(&self) -> serde_json::Value {
        let pending_restores = self
            .0
            .restores
            .try_lock()
            .ok()
            .map(|requests| requests.len());
        let Ok(state) = self.0.state.try_lock() else {
            return serde_json::json!({
                "available": false,
                "pending_restore_count": pending_restores,
            });
        };
        let fault = state.fault.as_ref().map(|fault| {
            let phase = match fault.phase {
                Phase::Pending => "pending",
                Phase::Entering => "entering",
                Phase::Entered => "entered",
                Phase::Restoring => "restoring",
            };
            serde_json::json!({
                "owner": fault.id,
                "phase": phase,
                "member_count": fault.members.len(),
            })
        });
        serde_json::json!({
            "available": true,
            "closed": state.closed,
            "fault": fault,
            "pending_restore_count": pending_restores,
            "proxy_owners": state.proxies.len(),
            "listener_task_owners": state.proxies.values().filter(|proxy| proxy.task.is_some()).count(),
            "finished_listener_tasks": state.proxies.values().filter(|proxy| proxy.task.as_ref().is_some_and(JoinHandle::is_finished)).count(),
            "stream_task_owners": state.proxies.values().map(|proxy| proxy.stream_tasks.load(Ordering::Acquire)).sum::<usize>(),
            "fatal_count": state.fatal.len(),
        })
    }

    fn healthy(state: &State) -> Result<()> {
        ensure!(
            state.fatal.is_empty(),
            "fatal transport failure: {}",
            state.fatal.join("; ")
        );
        Ok(())
    }
}

impl Network {
    fn failed(state: &mut State, error: anyhow::Error) -> anyhow::Error {
        let message = format!("{error:#}");
        state.fatal.push(message.clone());
        if let Some(fault) = &state.fault {
            fault.status.send_replace(FaultStatus::Failed(message));
        }
        error
    }

    fn reserve(&self, state: &mut State, members: &[String], phase: Phase) -> Result<Isolation> {
        if state.fault.is_some() {
            return Err(FaultAlreadyActive.into());
        }
        Self::healthy(state)?;
        ensure!(!state.closed, "test transport closed");
        let unique: BTreeSet<_> = members.iter().cloned().collect();
        ensure!(
            !unique.is_empty() && unique.len() == members.len(),
            "invalid isolation group"
        );
        ensure!(
            unique.iter().all(|member| !member.is_empty()),
            "empty isolation member"
        );
        state.next_owner = state
            .next_owner
            .checked_add(1)
            .context("transport owner ID exhausted")?;
        let id = state.next_owner;
        let (status, receiver) = watch::channel(FaultStatus::Pending);
        state.fault = Some(Fault {
            id,
            members: unique,
            phase,
            status,
        });
        Ok(Isolation {
            network: self.0.clone(),
            id,
            status: receiver,
            released: false,
            deadline: None,
        })
    }

    pub async fn isolate(&self, members: &[String]) -> Result<Isolation> {
        let mut state = self.0.state.lock().await;
        let token = self.reserve(&mut state, members, Phase::Entering)?;
        state.isolated = state.fault.as_ref().unwrap().members.clone();
        Self::enter(&mut state).await?;
        Ok(token)
    }

    pub async fn arm_next_build_dial(&self, member: &str) -> Result<Isolation> {
        let mut state = self.0.state.lock().await;
        self.reserve(&mut state, &[member.to_owned()], Phase::Pending)
    }

    async fn enter(state: &mut State) -> Result<()> {
        if let Err(error) = Self::update_gates(state).await {
            return Err(Self::failed(state, error));
        }
        let fault = state
            .fault
            .as_mut()
            .context("missing entering transport owner")?;
        fault.phase = Phase::Entered;
        fault.status.send_replace(FaultStatus::Entered);
        Ok(())
    }

    async fn restore(state: &mut State, id: u64) -> Result<()> {
        let Some(_) = state.fault.as_mut().filter(|fault| fault.id == id) else {
            return Ok(());
        };
        Self::healthy(state)?;
        state.fault.as_mut().unwrap().phase = Phase::Restoring;
        state.isolated.clear();
        if let Err(error) = Self::update_gates(state).await {
            return Err(Self::failed(state, error));
        }
        let fault = state.fault.take().unwrap();
        fault.status.send_replace(FaultStatus::Healed);
        Ok(())
    }

    async fn pending_restores(&self, state: &mut State) -> Result<()> {
        let requests = std::mem::take(&mut *self.0.restores.lock().unwrap());
        for id in requests {
            Self::restore(state, id).await?;
        }
        if let Some(fault) = &state.fault {
            match fault.phase {
                Phase::Restoring => Self::restore(state, fault.id).await?,
                Phase::Entering => Self::enter(state).await?,
                _ => {}
            }
        }
        Self::healthy(state)
    }

    pub async fn check(&self) -> Result<()> {
        let mut state = self.0.state.lock().await;
        self.pending_restores(&mut state).await?;
        if state
            .proxies
            .values()
            .any(|proxy| proxy.task.as_ref().is_some_and(JoinHandle::is_finished))
        {
            return Err(Self::failed(
                &mut state,
                anyhow::anyhow!("proxy task stopped unexpectedly"),
            ));
        }
        Ok(())
    }

    pub async fn heal(&self, mut token: Isolation) -> Result<()> {
        ensure!(
            Arc::ptr_eq(&self.0, &token.network),
            InvalidIsolation("transport token belongs to another network")
        );
        let mut state = self.0.state.lock().await;
        ensure!(
            state
                .fault
                .as_ref()
                .is_some_and(|fault| fault.id == token.id),
            InvalidIsolation("stale transport token")
        );
        Self::restore(&mut state, token.id).await?;
        token.released = true;
        Ok(())
    }

    pub async fn isolated_members(&self, token: &Isolation) -> Result<Vec<String>> {
        ensure!(
            Arc::ptr_eq(&self.0, &token.network),
            InvalidIsolation("transport token belongs to another network")
        );
        let state = self.0.state.lock().await;
        Self::healthy(&state)?;
        let fault = state
            .fault
            .as_ref()
            .filter(|fault| fault.id == token.id)
            .ok_or(InvalidIsolation("stale transport token"))?;
        ensure!(
            fault.phase == Phase::Entered,
            InvalidIsolation("transport cut has not been acknowledged")
        );
        Ok(fault.members.iter().cloned().collect())
    }

    pub async fn route(&self, source: &str, destination: &str, endpoint: &str) -> Result<String> {
        self.route_inner(None, source, destination, endpoint).await
    }

    pub async fn route_data(
        &self,
        kind: TestingTcpRouteKind,
        source: &str,
        destination: &str,
        endpoint: &str,
    ) -> Result<String> {
        self.route_inner(Some(kind), source, destination, endpoint)
            .await
    }

    async fn route_inner(
        &self,
        kind: Option<TestingTcpRouteKind>,
        source: &str,
        destination: &str,
        endpoint: &str,
    ) -> Result<String> {
        let mut state = self.0.state.lock().await;
        self.pending_restores(&mut state).await?;
        ensure!(!state.closed, "test transport closed");
        let endpoint = endpoint_key(endpoint);
        let uri: http::Uri = endpoint.parse()?;
        ensure!(
            uri.scheme_str() == Some("http") && uri.host() == Some("127.0.0.1"),
            "expected loopback HTTP endpoint"
        );
        let backend = uri
            .authority()
            .context("TCP authority missing")?
            .to_string();
        ensure!(
            state
                .owners
                .get(&endpoint)
                .is_none_or(|owner| owner == destination),
            "endpoint reused by another live instance"
        );
        state
            .owners
            .insert(endpoint.clone(), destination.to_owned());
        let entering = kind == Some(TestingTcpRouteKind::Build)
            && state.fault.as_ref().is_some_and(|fault| {
                fault.phase == Phase::Pending
                    && (fault.members.contains(source) || fault.members.contains(destination))
            });
        if entering {
            state.fault.as_mut().unwrap().phase = Phase::Entering;
            state.isolated = state.fault.as_ref().unwrap().members.clone();
        }
        let key = (source.to_owned(), endpoint.clone());
        if !state.proxies.contains_key(&key) {
            let blocked = Self::blocked(&state, source, &endpoint);
            let proxy = match Proxy::new(backend, blocked).await {
                Ok(proxy) => proxy,
                Err(error) => return Err(Self::failed(&mut state, error)),
            };
            state.proxies.insert(key.clone(), proxy);
        }
        if entering {
            Self::enter(&mut state).await?;
        }
        Ok(state.proxies[&key].address.clone())
    }

    fn blocked(state: &State, source: &str, endpoint: &str) -> bool {
        state.isolated.contains(source)
            || state
                .owners
                .get(endpoint)
                .is_some_and(|owner| state.isolated.contains(owner))
    }

    async fn update_gates(state: &State) -> Result<()> {
        for ((source, endpoint), proxy) in &state.proxies {
            proxy.cut(Self::blocked(state, source, endpoint)).await?;
        }
        Ok(())
    }

    pub async fn is_isolated(&self, uid: &str) -> bool {
        self.0
            .state
            .lock()
            .await
            .fault
            .as_ref()
            .is_some_and(|fault| fault.phase != Phase::Pending && fault.members.contains(uid))
    }

    pub async fn physical_route(&self, source: &str, endpoint: &str) -> Option<String> {
        self.0
            .state
            .lock()
            .await
            .proxies
            .get(&(source.to_owned(), endpoint_key(endpoint)))
            .map(|proxy| proxy.address.clone())
    }

    pub async fn forget(&self, uid: &str) -> Result<()> {
        let mut state = self.0.state.lock().await;
        self.pending_restores(&mut state).await?;
        let endpoints: BTreeSet<_> = state
            .owners
            .iter()
            .filter(|(_, owner)| owner.as_str() == uid)
            .map(|(endpoint, _)| endpoint.clone())
            .collect();
        let keys: Vec<_> = state
            .proxies
            .keys()
            .filter(|(source, endpoint)| source == uid || endpoints.contains(endpoint))
            .cloned()
            .collect();
        let mut errors = Vec::new();
        for key in &keys {
            if let Err(error) = state.proxies.get_mut(key).unwrap().finish().await {
                errors.push(format!("{error:#}"));
            }
        }
        for key in keys {
            state.proxies.remove(&key);
        }
        state.owners.retain(|_, owner| owner != uid);
        if !errors.is_empty() {
            return Err(Self::failed(
                &mut state,
                anyhow::anyhow!("transport retirement: {}", errors.join("; ")),
            ));
        }
        Ok(())
    }

    pub async fn finish(&self) -> Result<()> {
        let mut state = self.0.state.lock().await;
        state.closed = true;
        state.isolated.clear();
        let mut errors = Vec::new();
        for proxy in state.proxies.values_mut() {
            if let Err(error) = proxy.finish().await {
                errors.push(format!("{error:#}"));
            }
        }
        state.fatal.extend(errors);
        state.proxies.clear();
        state.owners.clear();
        if let Some(fault) = state.fault.take() {
            let status = if state.fatal.is_empty() {
                FaultStatus::Healed
            } else {
                FaultStatus::Failed(state.fatal.join("; "))
            };
            fault.status.send_replace(status);
        }
        self.0.restores.lock().unwrap().clear();
        Self::healthy(&state)
    }

    pub async fn assert_drained(&self) -> Result<()> {
        let state = self.0.state.lock().await;
        ensure!(
            state.closed
                && state.proxies.is_empty()
                && state.owners.is_empty()
                && state.fault.is_none()
                && self.0.restores.lock().unwrap().is_empty(),
            "transport owners not drained"
        );
        Self::healthy(&state)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn already_active<T>(result: Result<T>) {
        let Err(error) = result else {
            panic!("second transport fault accepted");
        };
        assert!(
            error.downcast_ref::<FaultAlreadyActive>().is_some(),
            "{error:#}"
        );
    }

    fn invalid_isolation<T>(result: Result<T>) {
        let Err(error) = result else {
            panic!("invalid isolation token accepted");
        };
        assert!(error.is::<InvalidIsolation>(), "{error:#}");
    }

    async fn connect(address: &str, backend: &TcpListener) -> Result<(TcpStream, TcpStream)> {
        let uri: http::Uri = address.parse()?;
        let client = TcpStream::connect(uri.authority().unwrap().as_str()).await?;
        let (server, _) = tokio::time::timeout(ACK_TIMEOUT, backend.accept()).await??;
        Ok((client, server))
    }

    async fn closed(stream: &mut TcpStream) -> Result<()> {
        ensure!(
            tokio::time::timeout(ACK_TIMEOUT, stream.read_u8())
                .await?
                .is_err(),
            "stream remained open"
        );
        Ok(())
    }

    #[tokio::test]
    async fn exclusive_manual_and_armed_requests_do_not_mutate_existing_owner() -> Result<()> {
        let network = Network::default();
        for armed in [false, true] {
            let mut token = if armed {
                network.arm_next_build_dial("first").await?
            } else {
                network.isolate(&["first".into(), "second".into()]).await?
            };
            let id = token.id;
            already_active(network.isolate(&["unrelated".into()]).await);
            already_active(network.arm_next_build_dial("unrelated").await);
            already_active(network.isolate(&[]).await);
            let state = network.0.state.lock().await;
            assert_eq!(state.fault.as_ref().unwrap().id, id);
            assert!(!state.isolated.contains("unrelated"));
            drop(state);
            if armed {
                invalid_isolation(network.isolated_members(&token).await);
                assert!(!network.is_isolated("first").await);
            } else {
                token.entered().await?;
                assert_eq!(network.isolated_members(&token).await?, ["first", "second"]);
            }
            network.heal(token).await?;
        }
        network.finish().await?;
        network.assert_drained().await
    }

    #[tokio::test]
    async fn invalid_groups_and_endpoint_reuse_are_atomic() -> Result<()> {
        let network = Network::default();
        for invalid in [
            vec![],
            vec!["valid".into(), "".into()],
            vec!["same".into(), "same".into()],
        ] {
            assert!(network.isolate(&invalid).await.is_err());
            let state = network.0.state.lock().await;
            assert!(state.fault.is_none() && state.isolated.is_empty());
        }
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let endpoint = format!("http://{}", listener.local_addr()?);
        let address = network.route("source", "owner", &endpoint).await?;
        assert!(network.route("other", "intruder", &endpoint).await.is_err());
        let state = network.0.state.lock().await;
        assert_eq!(state.owners[&endpoint], "owner");
        assert_eq!(state.proxies.len(), 1);
        drop(state);
        assert_eq!(
            network.physical_route("source", &endpoint).await,
            Some(address)
        );
        network.finish().await?;
        network.assert_drained().await
    }

    #[tokio::test]
    async fn only_matching_build_enters_and_heal_cancels_pending_or_entered_owner() -> Result<()> {
        let network = Network::default();
        let backend = TcpListener::bind("127.0.0.1:0").await?;
        let endpoint = format!("http://{}", backend.local_addr()?);
        for member in ["source", "destination"] {
            let mut token = network.arm_next_build_dial(member).await?;
            network.route("source", "destination", &endpoint).await?;
            network
                .route_data(
                    TestingTcpRouteKind::Replication,
                    "source",
                    "destination",
                    &endpoint,
                )
                .await?;
            network
                .route_data(
                    TestingTcpRouteKind::Build,
                    "unrelated",
                    "destination-other",
                    "http://127.0.0.1:1",
                )
                .await?;
            assert!(!network.is_isolated(member).await);
            let address = network
                .route_data(
                    TestingTcpRouteKind::Build,
                    "source",
                    "destination",
                    &endpoint,
                )
                .await?;
            token.entered().await?;
            assert_eq!(network.isolated_members(&token).await?, [member]);
            let uri: http::Uri = address.parse()?;
            let mut client = TcpStream::connect(uri.authority().unwrap().as_str()).await?;
            closed(&mut client).await?;
            already_active(network.arm_next_build_dial("other").await);
            network.heal(token).await?;
            assert!(!network.is_isolated(member).await);
        }
        let token = network.arm_next_build_dial("source").await?;
        network.heal(token).await?;
        network
            .route_data(
                TestingTcpRouteKind::Build,
                "source",
                "destination",
                &endpoint,
            )
            .await?;
        assert!(!network.is_isolated("source").await);
        network.finish().await?;
        network.assert_drained().await
    }

    #[tokio::test]
    async fn dropped_tokens_retain_slot_until_ack_and_stale_restores_cannot_heal_new_owner()
    -> Result<()> {
        let network = Network::default();
        for armed in [true, false] {
            let token = if armed {
                network.arm_next_build_dial("old").await?
            } else {
                network.isolate(&["old".into()]).await?
            };
            let stale = token.id;
            drop(token);
            already_active(network.isolate(&["new".into()]).await);
            assert_eq!(
                network.0.state.lock().await.fault.as_ref().unwrap().id,
                stale
            );
            network.check().await?;
            assert!(!network.is_isolated("old").await);
            let new = network.isolate(&["new".into()]).await?;
            network.0.restores.lock().unwrap().push(stale);
            network.check().await?;
            assert_eq!(network.isolated_members(&new).await?, ["new"]);
            network.heal(new).await?;
        }
        network.finish().await?;
        network.assert_drained().await
    }

    #[tokio::test]
    async fn foreign_token_never_changes_other_network() -> Result<()> {
        let first = Network::default();
        let second = Network::default();
        let token = first.isolate(&["first".into()]).await?;
        let other = second.isolate(&["second".into()]).await?;
        invalid_isolation(second.isolated_members(&token).await);
        invalid_isolation(second.heal(token).await);
        assert!(second.is_isolated("second").await);
        first.check().await?;
        second.heal(other).await?;
        first.finish().await?;
        second.finish().await?;
        first.assert_drained().await?;
        second.assert_drained().await
    }

    #[tokio::test]
    async fn stale_token_validation_is_typed_and_cannot_modify_current_owner() -> Result<()> {
        let network = Network::default();
        let first = network.isolate(&["first".into()]).await?;
        let stale = Isolation {
            network: first.network.clone(),
            id: first.id,
            status: first.status.clone(),
            released: true,
            deadline: None,
        };
        network.heal(first).await?;
        let current = network.isolate(&["current".into()]).await?;
        invalid_isolation(network.isolated_members(&stale).await);
        invalid_isolation(network.heal(stale).await);
        assert_eq!(network.isolated_members(&current).await?, ["current"]);
        network.heal(current).await?;
        network.finish().await?;
        network.assert_drained().await
    }

    #[tokio::test]
    async fn diagnostics_are_nonblocking_redacted_and_track_owned_tasks() -> Result<()> {
        let network = Network::default();
        {
            let _state = network.0.state.lock().await;
            assert_eq!(network.diagnostics()["available"], false);
        }
        {
            let _restores = network.0.restores.lock().unwrap();
            let view = network.diagnostics();
            assert_eq!(view["available"], true);
            assert!(view["pending_restore_count"].is_null());
        }
        let backend = TcpListener::bind("127.0.0.1:0").await?;
        let endpoint = format!("http://{}", backend.local_addr()?);
        let address = network
            .route("private-member", "private-destination", &endpoint)
            .await?;
        let (mut client, mut server) = connect(&address, &backend).await?;
        assert_eq!(network.diagnostics()["stream_task_owners"], 1);
        let token = network.isolate(&["private-member".into()]).await?;
        let view = network.diagnostics();
        assert_eq!(view["proxy_owners"], 1);
        assert_eq!(view["listener_task_owners"], 1);
        assert_eq!(view["stream_task_owners"], 0);
        assert_eq!(view["fault"]["phase"], "entered");
        assert_eq!(view["fault"]["member_count"], 1);
        assert!(!view.to_string().contains("private-member"));
        assert!(!view.to_string().contains(&endpoint));
        closed(&mut client).await?;
        closed(&mut server).await?;
        drop(token);
        assert_eq!(network.diagnostics()["pending_restore_count"], 1);
        network.check().await?;
        assert!(network.diagnostics()["fault"].is_null());
        network.finish().await?;
        let view = network.diagnostics();
        assert_eq!(view["closed"], true);
        assert_eq!(view["proxy_owners"], 0);
        assert_eq!(view["listener_task_owners"], 0);
        assert_eq!(view["stream_task_owners"], 0);
        assert_eq!(view["pending_restore_count"], 0);
        network.assert_drained().await
    }

    #[tokio::test]
    async fn cuts_established_duplex_streams_by_source_and_destination_and_drains() -> Result<()> {
        let backend = TcpListener::bind("127.0.0.1:0").await?;
        let endpoint = format!("http://{}", backend.local_addr()?);
        let network = Network::default();
        let address = network.route("source", "destination", &endpoint).await?;
        let survivor_backend = TcpListener::bind("127.0.0.1:0").await?;
        let survivor_endpoint = format!("http://{}", survivor_backend.local_addr()?);
        let survivor_address = network
            .route("survivor-a", "survivor-b", &survivor_endpoint)
            .await?;
        let (mut survivor_client, mut survivor_server) =
            connect(&survivor_address, &survivor_backend).await?;
        for cut in ["source", "destination"] {
            let (mut client, mut server) = connect(&address, &backend).await?;
            client.write_all(b"a").await?;
            ensure!(server.read_u8().await? == b'a');
            server.write_all(b"b").await?;
            ensure!(client.read_u8().await? == b'b');
            let mut token = network.isolate(&[cut.into()]).await?;
            token.entered().await?;
            survivor_client.write_all(b"s").await?;
            ensure!(survivor_server.read_u8().await? == b's');
            closed(&mut client).await?;
            closed(&mut server).await?;
            let uri: http::Uri = address.parse()?;
            let mut fresh = TcpStream::connect(uri.authority().unwrap().as_str()).await?;
            closed(&mut fresh).await?;
            network.heal(token).await?;
            survivor_server.write_all(b"h").await?;
            ensure!(survivor_client.read_u8().await? == b'h');
        }
        let (mut client, mut server) = connect(&address, &backend).await?;
        client.write_all(b"r").await?;
        ensure!(server.read_u8().await? == b'r');
        network.finish().await?;
        closed(&mut client).await?;
        closed(&mut server).await?;
        closed(&mut survivor_client).await?;
        closed(&mut survivor_server).await?;
        assert!(
            TcpStream::connect(address.trim_start_matches("http://"))
                .await
                .is_err()
        );
        assert!(
            network
                .route("source", "destination", &endpoint)
                .await
                .is_err()
        );
        network.assert_drained().await
    }

    #[tokio::test]
    async fn entered_build_joins_existing_streams_and_drop_restores_before_reuse() -> Result<()> {
        let network = Network::default();
        let backend = TcpListener::bind("127.0.0.1:0").await?;
        let endpoint = format!("http://{}", backend.local_addr()?);
        let address = network.route("source", "destination", &endpoint).await?;
        let (mut client, mut server) = connect(&address, &backend).await?;
        let mut token = network.arm_next_build_dial("destination").await?;
        network
            .route_data(
                TestingTcpRouteKind::Build,
                "source",
                "destination",
                &endpoint,
            )
            .await?;
        token.entered().await?;
        closed(&mut client).await?;
        closed(&mut server).await?;
        drop(token);
        already_active(network.arm_next_build_dial("other").await);
        network.check().await?;
        let (mut client, mut server) = connect(&address, &backend).await?;
        client.write_all(b"r").await?;
        assert_eq!(server.read_u8().await?, b'r');
        network.forget("destination").await?;
        closed(&mut client).await?;
        closed(&mut server).await?;
        assert!(network.physical_route("source", &endpoint).await.is_none());
        network.finish().await?;
        network.assert_drained().await
    }

    #[tokio::test]
    async fn failed_gates_are_fatal_and_keep_owner_until_teardown() -> Result<()> {
        let network = Network::default();
        network
            .route("source", "destination", "http://127.0.0.1:1")
            .await?;
        {
            let state = network.0.state.lock().await;
            let proxy = state.proxies.values().next().unwrap();
            proxy.task.as_ref().unwrap().abort();
        }
        tokio::task::yield_now().await;
        assert!(network.isolate(&["source".into()]).await.is_err());
        assert!(network.0.state.lock().await.fault.is_some());
        already_active(network.arm_next_build_dial("other").await);
        let error = network.check().await.unwrap_err();
        assert!(!error.is::<InvalidIsolation>());
        assert!(network.finish().await.is_err());
        let state = network.0.state.lock().await;
        assert!(state.proxies.is_empty() && state.fault.is_none());
        assert!(state.fatal.iter().any(|error| error.contains("cancelled")));
        Ok(())
    }

    #[tokio::test]
    async fn stream_panics_are_reported_after_all_streams_join() -> Result<()> {
        let mut streams = StreamOwners::default();
        streams.tasks.spawn(async {
            panic!("forwarder sentinel");
        });
        streams.tasks.spawn(std::future::pending::<()>());
        tokio::task::yield_now().await;
        let error = join_streams(&mut streams, true).await.unwrap_err();
        assert!(format!("{error:#}").contains("forwarder sentinel"));
        assert!(streams.tasks.is_empty());
        assert!(
            format!("{:#}", join_streams(&mut streams, true).await.unwrap_err())
                .contains("forwarder sentinel")
        );
        Ok(())
    }

    #[tokio::test]
    async fn teardown_cancels_outstanding_tokens_and_late_drop_does_not_reopen_cleanup()
    -> Result<()> {
        for armed in [false, true] {
            let network = Network::default();
            let mut token = if armed {
                network.arm_next_build_dial("member").await?
            } else {
                network.isolate(&["member".into()]).await?
            };
            network.finish().await?;
            assert!(token.entered().await.is_err());
            assert!(network.isolated_members(&token).await.is_err());
            drop(token);
            network.assert_drained().await?;
        }
        Ok(())
    }

    #[tokio::test]
    async fn cancelled_cut_and_heal_keep_joinable_ownership() -> Result<()> {
        let network = Network::default();
        network
            .route("source", "destination", "http://127.0.0.1:1")
            .await?;
        let members = ["source".to_owned()];
        let mut cutting = Box::pin(network.isolate(&members));
        assert!(futures::poll!(&mut cutting).is_pending());
        drop(cutting);
        already_active(network.arm_next_build_dial("other").await);
        network.check().await?;
        assert!(!network.is_isolated("source").await);

        let token = network.isolate(&members).await?;
        let mut healing = Box::pin(network.heal(token));
        assert!(futures::poll!(&mut healing).is_pending());
        drop(healing);
        already_active(network.arm_next_build_dial("other").await);
        network.check().await?;
        assert!(!network.is_isolated("source").await);
        let token = network.arm_next_build_dial("source").await?;
        network.heal(token).await?;

        let mut finishing = Box::pin(network.finish());
        assert!(futures::poll!(&mut finishing).is_pending());
        drop(finishing);
        assert!(network.assert_drained().await.is_err());
        network.finish().await?;
        network.assert_drained().await
    }

    #[tokio::test]
    async fn direct_entry_wait_obeys_scenario_deadline_without_releasing_owner() -> Result<()> {
        let network = Network::default();
        let mut token = network
            .arm_next_build_dial("member")
            .await?
            .with_deadline(tokio::time::Instant::now() + Duration::from_millis(10));
        let id = token.id;
        let error = token.entered().await.unwrap_err();
        assert_eq!(
            super::super::TestError::from(error).kind(),
            super::super::ErrorKind::Deadline
        );
        already_active(network.isolate(&["other".into()]).await);
        assert_eq!(network.0.state.lock().await.fault.as_ref().unwrap().id, id);
        assert!(!network.is_isolated("member").await);
        network.heal(token).await?;

        let mut token = network
            .isolate(&["member".into()])
            .await?
            .with_deadline(tokio::time::Instant::now() + ACK_TIMEOUT);
        token.entered().await?;
        token = token.with_deadline(tokio::time::Instant::now());
        let error = token.entered().await.unwrap_err();
        assert_eq!(
            super::super::TestError::from(error).kind(),
            super::super::ErrorKind::Deadline
        );
        assert_eq!(network.isolated_members(&token).await?, ["member"]);
        already_active(network.arm_next_build_dial("other").await);
        network.heal(token).await?;
        network.finish().await?;
        network.assert_drained().await
    }

    #[tokio::test]
    async fn scenario_timeout_of_cut_or_heal_retains_owner_until_gate_ack() -> Result<()> {
        for healing in [false, true] {
            let network = Network::default();
            let members = ["source".to_owned()];
            let token = if healing {
                Some(network.isolate(&members).await?)
            } else {
                None
            };
            let proxy = Proxy::new("127.0.0.1:1".into(), healing).await?;
            let stalled = proxy.streams.clone().lock_owned().await;
            {
                let mut state = network.0.state.lock().await;
                state
                    .owners
                    .insert("http://127.0.0.1:1".into(), "destination".into());
                state
                    .proxies
                    .insert(("source".into(), "http://127.0.0.1:1".into()), proxy);
            }
            let operation = async {
                match token {
                    Some(token) => network.heal(token).await,
                    None => network.isolate(&members).await.map(|_| ()),
                }
            };
            assert!(
                tokio::time::timeout_at(
                    tokio::time::Instant::now() + Duration::from_millis(10),
                    operation,
                )
                .await
                .is_err()
            );
            assert!(network.0.state.lock().await.fault.is_some());
            assert!(network.is_isolated("source").await);
            already_active(network.arm_next_build_dial("other").await);
            drop(stalled);
            network.check().await?;
            assert!(!network.is_isolated("source").await);
            let token = network.isolate(&["other".into()]).await?;
            network.heal(token).await?;
            network.finish().await?;
            network.assert_drained().await?;
        }
        Ok(())
    }
}
