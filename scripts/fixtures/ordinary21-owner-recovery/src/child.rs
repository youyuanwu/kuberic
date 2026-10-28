use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::net::SocketAddr;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use anyhow::{Context, bail, ensure};
use async_trait::async_trait;
use kuberic_runtime::application::{OpenContext, RoleChange};
use kuberic_runtime::host::{
    ApplicationStorageState, ReplicaEndpointResolver, ReplicaHost, ReplicaProcessConfig,
    RunningReplica,
};
use kuberic_runtime::protocol::types::{
    PodUid, ProcessSessionId, PvcUid, ReplicaId, ReplicaIdentity, ReplicaRole, ResourceUid,
};
use kuberic_runtime::replicator::{
    DefaultReplicatorFactory, Replicator, ReplicatorFactory, ReplicatorFactoryContext,
    ReplicatorInterfaces, ReplicatorSettings, StatefulServicePartition,
};
use kuberic_runtime::{RuntimeError, StateProvider, StatefulServiceReplica};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{OnceCell, watch};
use tokio::task::JoinSet;

use crate::counter::{CounterService, CounterStore};
use crate::shared::{
    ChildCommand, ChildError, ChildErrorKind, ChildEvent, ChildReady, ChildReply, ParentReply,
    ParentRequest, PeerEndpoint, StableReplica, TOKEN_ENV, read_frame, write_frame,
};

const IO_BOUND: Duration = Duration::from_secs(10);
const MAX_CONNECTIONS: usize = 32;

#[derive(Clone)]
struct Events(Arc<Mutex<std::io::Stdout>>);

impl Events {
    fn emit(&self, event: ChildEvent) -> std::io::Result<()> {
        let mut output = self
            .0
            .lock()
            .map_err(|_| std::io::Error::other("stdout event writer poisoned"))?;
        serde_json::to_writer(&mut *output, &event)
            .map_err(|_| std::io::Error::other("stdout event serialization failed"))?;
        output.write_all(b"\n")?;
        output.flush()
    }
}

struct Config {
    root: PathBuf,
    stable: StableReplica,
    control_bind: SocketAddr,
    replication_bind: SocketAddr,
    data_bind: SocketAddr,
    hold_first_open: bool,
    peers: Vec<PeerEndpoint>,
}

impl Config {
    fn parse(args: impl Iterator<Item = String>) -> anyhow::Result<Self> {
        let mut args = args;
        ensure!(args.next().as_deref() == Some("child"), "child mode is required");
        let mut values = BTreeMap::new();
        let mut hold_first_open = false;
        while let Some(key) = args.next() {
            if key == "--hold-first-open" {
                ensure!(!hold_first_open, "duplicate hold flag");
                hold_first_open = true;
                continue;
            }
            ensure!(
                [
                    "--root", "--resource", "--replica", "--pod", "--pvc", "--generation",
                    "--spec-generation", "--control-bind", "--replication-bind", "--data-bind",
                    "--peers-json",
                ]
                .contains(&key.as_str()),
                "unknown child option"
            );
            let value = args.next().context("missing child option value")?;
            ensure!(values.insert(key, value).is_none(), "duplicate child option");
        }
        let mut required = |key: &str| values.remove(key).context("missing required child option");
        let root = PathBuf::from(required("--root")?);
        let resource = required("--resource")?;
        let replica_id = ReplicaId::new(required("--replica")?.parse()?);
        let pod = required("--pod")?;
        let pvc = required("--pvc")?;
        let control_bind: SocketAddr = required("--control-bind")?.parse()?;
        let replication_bind: SocketAddr = required("--replication-bind")?.parse()?;
        let data_bind: SocketAddr = required("--data-bind")?.parse()?;
        let generation: u64 = values
            .remove("--generation")
            .unwrap_or_else(|| "1".into())
            .parse()?;
        let spec_generation: u64 = values
            .remove("--spec-generation")
            .unwrap_or_else(|| "1".into())
            .parse()?;
        let peers: Vec<PeerEndpoint> = serde_json::from_str(
            &values.remove("--peers-json").unwrap_or_else(|| "[]".into()),
        )
        .context("invalid public peer endpoint list")?;
        ensure!(
            !resource.is_empty() && !pod.is_empty() && !pvc.is_empty()
                && replica_id.value() > 0 && generation > 0 && spec_generation > 0,
            "invalid stable identity"
        );
        for address in [control_bind, replication_bind, data_bind] {
            ensure!(address.ip().is_loopback(), "fixture sockets must be loopback");
        }
        ensure!(
            root.is_absolute()
                && root.parent().is_some()
                && !root.starts_with("/tmp")
                && !root.starts_with("/var/tmp")
                && !root.components().any(|part| matches!(part, Component::ParentDir)),
            "root must be an existing absolute owned non-temporary directory"
        );
        ensure!(root.is_dir() && root.canonicalize()? == root, "root must be canonical");
        for path in [root.join("agent"), root.join("application")] {
            match std::fs::symlink_metadata(path) {
                Ok(metadata) => ensure!(metadata.is_dir(), "owned subroot is not a real directory"),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        Ok(Self {
            root,
            stable: StableReplica {
                resource_uid: ResourceUid::new(resource),
                replica_id,
                pod_uid: PodUid::new(pod),
                pvc_uid: PvcUid::new(pvc),
                generation,
                spec_generation,
            },
            control_bind,
            replication_bind,
            data_bind,
            hold_first_open,
            peers,
        })
    }
}

struct Resolver(BTreeMap<ReplicaIdentity, PeerEndpoint>);

impl Resolver {
    fn new(peers: Vec<PeerEndpoint>, local: PeerEndpoint) -> anyhow::Result<Self> {
        let mut entries = BTreeMap::new();
        let mut ids = BTreeSet::new();
        for peer in peers {
            ensure!(ids.insert(peer.identity.replica_id), "duplicate peer replica");
            ensure!(
                peer.identity.replica_id.value() > 0
                    && !peer.identity.instance_id.is_empty()
                    && !peer.identity.agent_generation.is_empty(),
                "incomplete peer identity"
            );
            validate_endpoint(&peer.control_endpoint)?;
            if !peer.replication_endpoint.is_empty() {
                validate_endpoint(&peer.replication_endpoint)?;
            }
            entries.insert(peer.identity.clone(), peer);
        }
        if let Some(peer) = entries.get(&local.identity) {
            ensure!(
                peer.control_endpoint == local.control_endpoint,
                "local control address differs"
            );
        } else {
            ensure!(
                !ids.contains(&local.identity.replica_id),
                "local peer identity differs"
            );
        }
        entries.insert(local.identity.clone(), local);
        Ok(Self(entries))
    }
}

fn validate_endpoint(endpoint: &str) -> anyhow::Result<()> {
    let address: SocketAddr = endpoint
        .strip_prefix("http://")
        .context("peer endpoint must use fixture http")?
        .parse()?;
    ensure!(
        address.ip().is_loopback() && address.port() != 0,
        "invalid peer endpoint"
    );
    Ok(())
}

impl ReplicaEndpointResolver for Resolver {
    fn control_endpoint(&self, identity: &ReplicaIdentity) -> String {
        self.0
            .get(identity)
            .map(|peer| peer.control_endpoint.clone())
            .unwrap_or_default()
    }

    fn replication_endpoint(&self, identity: &ReplicaIdentity) -> String {
        self.0
            .get(identity)
            .map(|peer| peer.replication_endpoint.clone())
            .unwrap_or_default()
    }
}

#[derive(Clone, Copy)]
enum GateState {
    Armed,
    Held,
    Released,
    Cancelled,
}

struct Application {
    root: PathBuf,
    address: String,
    inner: OnceCell<Arc<CounterService>>,
    hold_first: AtomicBool,
    stopped: AtomicBool,
    gate: watch::Sender<GateState>,
    process: OnceLock<ProcessSessionId>,
    events: Events,
}

struct ProbeFactory {
    callbacks: Arc<AtomicUsize>,
    delegate: bool,
}

#[async_trait]
impl ReplicatorFactory for ProbeFactory {
    async fn create_replicator(
        &self,
        context: ReplicatorFactoryContext,
        provider: Option<Arc<dyn StateProvider>>,
        settings: ReplicatorSettings,
    ) -> kuberic_runtime::Result<ReplicatorInterfaces> {
        self.callbacks.fetch_add(1, Ordering::SeqCst);
        if self.delegate {
            DefaultReplicatorFactory::new().create_replicator(context, provider, settings).await
        } else {
            Err(RuntimeError::Application("independent probe callback entered".into()))
        }
    }
}

async fn probe_bound_overrides(partition: &StatefulServicePartition) -> kuberic_runtime::Result<Vec<String>> {
    let callbacks = Arc::new(AtomicUsize::new(0));
    let mut variants = Vec::new();
    for delegate in [false, true] {
        for builtin_first in [false, true] {
            let factory = Arc::new(ProbeFactory { callbacks: callbacks.clone(), delegate });
            let selected = if builtin_first {
                partition.with_builtin_replicator().with_factory(factory)
            } else {
                partition.with_factory(factory)
            };
            let result = selected.clone().create_replicator(None, None).await;
            if !matches!(result, Err(RuntimeError::AuthorityMismatch(ref message))
                if message == "host-bound builtin construction rejects factory overrides")
                || callbacks.load(Ordering::SeqCst) != 0
            {
                return Err(RuntimeError::Application("bound override crossed pre-construction rejection".into()));
            }
            variants.push(format!("{}-{}-clone",
                if delegate { "delegating-wrapper" } else { "independent" },
                if builtin_first { "builtin-then-override" } else { "override" }));
        }
    }
    Ok(variants)
}

impl Application {
    fn counter(&self) -> kuberic_runtime::Result<&Arc<CounterService>> {
        self.inner.get().ok_or(RuntimeError::NotOpen)
    }

    fn release(&self) -> Result<bool, ChildError> {
        let mut outcome = Err(ChildError {
            kind: ChildErrorKind::InvalidState,
            message: "first Open is not held".into(),
        });
        self.gate.send_if_modified(|state| match state {
            GateState::Held => {
                *state = GateState::Released;
                outcome = Ok(true);
                true
            }
            GateState::Released => {
                outcome = Ok(false);
                false
            }
            _ => false,
        });
        outcome
    }

    fn cancel(&self) {
        self.stopped.store(true, Ordering::Release);
        self.gate.send_replace(GateState::Cancelled);
    }
}

#[async_trait]
impl StatefulServiceReplica for Application {
    async fn open(
        self: Arc<Self>,
        context: OpenContext,
    ) -> kuberic_runtime::Result<Arc<dyn Replicator>> {
        if self.hold_first.swap(false, Ordering::AcqRel) {
            let variants = probe_bound_overrides(&context.partition).await?;
            self.events.emit(ChildEvent::BoundFactoryOverridesRejected {
                process_id: std::process::id(), callbacks: 0, variants,
            }).map_err(|error| RuntimeError::Application(error.to_string()))?;
            let mut gate = self.gate.subscribe();
            let held = self.gate.send_if_modified(|state| {
                if matches!(state, GateState::Armed) {
                    *state = GateState::Held;
                    true
                } else {
                    false
                }
            });
            if !held {
                return Err(RuntimeError::OperationCancelled);
            }
            self.events
                .emit(ChildEvent::OpenHeld {
                    process_id: std::process::id(),
                    identity: context.identity.clone(),
                    mode: context.mode,
                    process_session_id: self.process.get().cloned(),
                    // OpenContext does not publicly expose the creation identity.
                    replicator_creation_id: None,
                })
                .map_err(|error| RuntimeError::Application(error.to_string()))?;
            loop {
                let state = *gate.borrow_and_update();
                match state {
                    GateState::Released => break,
                    GateState::Cancelled => return Err(RuntimeError::OperationCancelled),
                    GateState::Armed | GateState::Held => {}
                }
                gate.changed()
                    .await
                    .map_err(|_| RuntimeError::OperationCancelled)?;
            }
        }
        if self.stopped.load(Ordering::Acquire) {
            return Err(RuntimeError::OperationCancelled);
        }
        let counter = self
            .inner
            .get_or_try_init(|| async {
                let (store, _) = CounterStore::open(&self.root)?;
                Ok::<_, RuntimeError>(Arc::new(CounterService::new(store, self.address.clone())))
            })
            .await?
            .clone();
        counter.open(context).await
    }

    async fn change_role(&self, role: ReplicaRole) -> kuberic_runtime::Result<RoleChange> {
        self.counter()?.change_role(role).await
    }

    async fn close(&self) -> kuberic_runtime::Result<()> {
        self.cancel();
        match self.inner.get() {
            Some(counter) => counter.close().await,
            None => Ok(()),
        }
    }

    fn abort(&self) {
        self.cancel();
        if let Some(counter) = self.inner.get() {
            counter.abort();
        }
    }
}

fn runtime_error(error: RuntimeError, token: &str) -> ChildError {
    let kind = match &error {
        RuntimeError::NotOpen => ChildErrorKind::NotOpen,
        RuntimeError::NotPrimary => ChildErrorKind::NotPrimary,
        RuntimeError::ReadClosed(_) => ChildErrorKind::ReadClosed,
        RuntimeError::WriteClosed(_) => ChildErrorKind::WriteClosed,
        RuntimeError::Closed => ChildErrorKind::Closed,
        RuntimeError::OperationCancelled => ChildErrorKind::OperationCancelled,
        _ => ChildErrorKind::Runtime,
    };
    ChildError {
        kind,
        message: error.to_string().replace(token, "[redacted]"),
    }
}

async fn connection(
    mut socket: TcpStream,
    application: Arc<Application>,
    token: Arc<str>,
    mut stopping: watch::Receiver<bool>,
) -> anyhow::Result<bool> {
    let request: ParentRequest = tokio::select! {
        _ = stopping.wait_for(|stopped| *stopped) => return Ok(false),
        result = tokio::time::timeout(IO_BOUND, read_frame(&mut socket)) => result??,
    };
    let mut shutdown = false;
    let result = if request.bearer_token != token.as_ref() {
        Err(ChildError {
            kind: ChildErrorKind::Unauthorized,
            message: "unauthorized".into(),
        })
    } else {
        match request.command {
            ChildCommand::ReleaseOpen => application
                .release()
                .map(|released| ChildReply::OpenReleased { released }),
            ChildCommand::Shutdown => {
                shutdown = true;
                Ok(ChildReply::ShutdownRequested)
            }
            command => match application.counter() {
                Err(error) => Err(runtime_error(error, &token)),
                Ok(counter) => {
                    match command {
                        ChildCommand::Set { value } => counter
                            .set(value)
                            .await
                            .map(|lsn| ChildReply::SetAcknowledged { lsn }),
                        ChildCommand::ServingRead => counter
                            .read()
                            .await
                            .map(|value| ChildReply::ServingRead { value }),
                        ChildCommand::CommittedValueDiagnostic => counter
                            .committed_value()
                            .map(|value| ChildReply::CommittedValueDiagnostic { value }),
                        _ => unreachable!("scheduling commands handled above"),
                    }
                    .map_err(|error| runtime_error(error, &token))
                }
            },
        }
    };
    let reply = ParentReply {
        request_id: request.request_id,
        result,
    };
    tokio::time::timeout(IO_BOUND, write_frame(&mut socket, &reply)).await??;
    Ok(shutdown)
}

fn application_empty(root: &Path) -> std::io::Result<bool> {
    match std::fs::read_dir(root) {
        Ok(mut entries) => entries.next().transpose().map(|entry| entry.is_none()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(true),
        Err(error) => Err(error),
    }
}

pub async fn run(args: impl Iterator<Item = String>) -> anyhow::Result<()> {
    let events = Events(Arc::new(Mutex::new(std::io::stdout())));
    let token =
        std::env::var(TOKEN_ENV).context("private credential environment variable required")?;
    ensure!(
        !token.is_empty() && token.bytes().all(|byte| byte.is_ascii_graphic()),
        "credential must be nonempty visible ASCII"
    );
    let result = run_owned(args, Arc::from(token.as_str()), events.clone()).await;
    if let Err(error) = &result {
        let _ = events.emit(ChildEvent::Failed {
            message: error.to_string().replace(&token, "[redacted]"),
        });
    }
    result
}

async fn run_owned(
    args: impl Iterator<Item = String>,
    token: Arc<str>,
    events: Events,
) -> anyhow::Result<()> {
    let config = Config::parse(args)?;
    let control = TcpListener::bind(config.control_bind).await?;
    let data = TcpListener::bind(config.data_bind).await?;
    let control_endpoint = format!("http://{}", control.local_addr()?);
    let data_endpoint = data.local_addr()?;
    let agent_root = config.root.join("agent");
    let application_root = config.root.join("application");
    let application_was_empty = application_empty(&application_root)?;
    let application = Arc::new(Application {
        root: application_root.clone(),
        address: format!("http://{}", config.replication_bind),
        inner: OnceCell::new(),
        hold_first: AtomicBool::new(config.hold_first_open),
        stopped: AtomicBool::new(false),
        gate: watch::channel(GateState::Armed).0,
        process: OnceLock::new(),
        events: events.clone(),
    });
    let resolver = Arc::new(Resolver::new(
        config.peers,
        PeerEndpoint {
            identity: config.stable.identity(),
            control_endpoint: control_endpoint.clone(),
            replication_endpoint: String::new(),
        },
    )?);
    let host = ReplicaHost::new(
        ReplicaProcessConfig {
            resource_uid: config.stable.resource_uid.clone(),
            replica_id: config.stable.replica_id,
            pod_uid: config.stable.pod_uid.clone(),
            pvc_uid: config.stable.pvc_uid.clone(),
            data_root: agent_root.clone(),
            replication_address: config.replication_bind,
            bearer_token: token.to_string(),
            rpc_deadline: Duration::from_secs(5),
            transport_window_capacity: 64,
        },
        application.clone(),
        if application_was_empty {
            ApplicationStorageState::FreshEmpty
        } else {
            ApplicationStorageState::Established
        },
        resolver,
    )
    .with_public_v1_binding(config.stable.binding())
    .with_application_storage_paths(BTreeMap::from([(
        "application".into(),
        application_root.clone(),
    )]));
    events.emit(ChildEvent::IpcReady {
        ready: ChildReady {
            process_id: std::process::id(),
            root: config.root,
            agent_root,
            application_root,
            identity: config.stable.identity(),
            initialization_id: config.stable.initialization_id(),
            expected_bootstrap_history: config.stable.bootstrap_history(),
            expected_initial_storage: config.stable.initial_storage(),
            stable: config.stable,
            control_endpoint,
            data_endpoint,
            requested_replication_bind: config.replication_bind,
            application_was_empty,
            hold_first_open: config.hold_first_open,
        },
    })?;
    let (startup_stop, startup_stopping) = watch::channel(false);
    let (ipc_stop, ipc_stopping) = watch::channel(false);
    let mut startup = Some(Box::pin(host.start_with_shutdown(control, startup_stopping)));
    let mut running: Option<RunningReplica> = None;
    let mut connections: JoinSet<anyhow::Result<bool>> = JoinSet::new();
    let mut failures = Vec::new();
    loop {
        tokio::select! {
            result = async { startup.as_mut().expect("guarded startup").as_mut().await }, if startup.is_some() => {
                startup = None;
                match result {
                    Ok(Some(replica)) => {
                        running = Some(replica);
                        match running.as_ref().expect("just assigned").handle().diagnostics().await {
                            Ok(diagnostics) => {
                                let process = ProcessSessionId::new(diagnostics.process_session);
                                let _ = application.process.set(process.clone());
                                if let Err(error) = events.emit(ChildEvent::HostStarted {
                                    process_id: std::process::id(),
                                    process_session_id: process,
                                }) {
                                    failures.push(error.to_string());
                                    break;
                                }
                            }
                            Err(error) => { failures.push(error.to_string()); break; }
                        }
                    }
                    Ok(None) => { failures.push("startup stopped without a parent shutdown".into()); break; }
                    Err(error) => { failures.push(error.to_string()); break; }
                }
            }
            result = async { running.as_mut().expect("guarded replica").wait().await }, if running.is_some() => {
                running = None;
                if let Err(error) = result { failures.push(error.to_string()); }
                break;
            }
            accepted = data.accept(), if connections.len() < MAX_CONNECTIONS => {
                match accepted {
                    Ok((socket, _)) => {
                        connections.spawn(connection(socket, application.clone(), token.clone(), ipc_stopping.clone()));
                    }
                    Err(error) => { failures.push(error.to_string()); break; }
                }
            }
            joined = connections.join_next(), if !connections.is_empty() => {
                match joined {
                    Some(Ok(Ok(true))) => break,
                    Some(Ok(Ok(false))) => {}
                    Some(Ok(Err(error))) => {
                        if let Err(error) = events.emit(ChildEvent::IpcError { message: error.to_string().replace(token.as_ref(), "[redacted]") }) {
                            failures.push(error.to_string());
                            break;
                        }
                    }
                    Some(Err(error)) => { failures.push(error.to_string()); break; }
                    None => {}
                }
            }
        }
    }
    drop(data);
    ipc_stop.send_replace(true);
    application.cancel();
    startup_stop.send_replace(true);
    if let Some(startup) = startup {
        match startup.await {
            Ok(Some(replica)) => running = Some(replica),
            Ok(None) => {}
            Err(error) => failures.push(error.to_string()),
        }
    }
    if let Some(mut replica) = running {
        replica.shutdown();
        if let Err(error) = replica.wait().await {
            failures.push(error.to_string());
        }
    }
    if let Err(error) = application.close().await {
        failures.push(error.to_string());
    }
    while let Some(joined) = connections.join_next().await {
        match joined {
            Ok(Ok(_)) => {}
            Ok(Err(error)) => {
                if let Err(error) = events.emit(ChildEvent::IpcError {
                    message: error.to_string().replace(token.as_ref(), "[redacted]"),
                }) {
                    failures.push(error.to_string());
                }
            }
            Err(error) => failures.push(error.to_string()),
        }
    }
    for failure in &mut failures {
        *failure = failure.replace(token.as_ref(), "[redacted]");
    }
    events.emit(ChildEvent::Stopped {
        process_id: std::process::id(),
        failures: failures.clone(),
    })?;
    if !failures.is_empty() {
        bail!("{}", failures.join("; "));
    }
    Ok(())
}
