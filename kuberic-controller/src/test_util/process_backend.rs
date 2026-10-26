//! A test-only physical member boundary. Stdio bootstraps endpoints; the router
//! changes destinations only. Application data and native observations are
//! separate requests and never execute controller goals.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream as BlockingStream};
use std::path::PathBuf;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use kuberic_runtime::host::{
    KubernetesDnsResolver, ReplicaEndpointResolver, ReplicaHandle, ReplicaHost,
    ReplicaProcessConfig,
};
use kuberic_runtime::protocol::public_operations::PublicV1AgentReport;
use kuberic_runtime::protocol::types::{
    PodUid, ProcessSessionEndpoint, ProcessSessionId, PvcUid, ReplicaId, ReplicaIdentity,
    ReplicaRole, ReplicatorCreationId, ResourceUid,
};
use kuberic_runtime::replicator::{TestingTcpRoute, TestingTcpRouteKind};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;
use tokio::task::{JoinHandle, JoinSet};

use super::{ApplicationFactory, ReplicaContext};
use crate::crd::CONTROL_ADDRESS_ANNOTATION;
use crate::test_util::kube::{ApiServer, Kind};
use crate::test_util::transport::Network;

const LIMIT: usize = 1024 * 1024;
const REQUEST_CAPACITY: usize = 64;
const RPC_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Bootstrap {
    generation: String,
    parent_pid: u32,
    identity: ReplicaIdentity,
    resource_uid: ResourceUid,
    pvc_uid: PvcUid,
    data_root: PathBuf,
    environment: BTreeMap<String, String>,
    bearer_token: String,
    router: SocketAddr,
    namespace: String,
    transport_window_capacity: usize,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Ready {
    generation: String,
    pid: u32,
    control: String,
    data: SocketAddr,
    lifecycle: SocketAddr,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    generation: String,
    operation: String,
    payload: Value,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Reply {
    generation: String,
    outcome: std::result::Result<Value, String>,
}

fn encode(value: &impl Serialize) -> Result<Vec<u8>> {
    let bytes = serde_json::to_vec(value)?;
    ensure!(bytes.len() <= LIMIT, "member frame exceeds limit");
    Ok(bytes)
}

fn write_frame(stream: &mut impl Write, value: &impl Serialize) -> Result<()> {
    let bytes = encode(value)?;
    stream.write_all(&(bytes.len() as u32).to_be_bytes())?;
    stream.write_all(&bytes)?;
    stream.flush()?;
    Ok(())
}

fn read_frame<T: serde::de::DeserializeOwned>(stream: &mut impl Read) -> Result<T> {
    let mut size = [0; 4];
    stream.read_exact(&mut size)?;
    let size = u32::from_be_bytes(size) as usize;
    ensure!(size <= LIMIT, "member frame exceeds limit");
    let mut bytes = vec![0; size];
    stream.read_exact(&mut bytes)?;
    Ok(serde_json::from_slice(&bytes)?)
}

async fn send_frame(stream: &mut (impl AsyncWrite + Unpin), value: &impl Serialize) -> Result<()> {
    let bytes = encode(value)?;
    stream.write_u32(bytes.len() as u32).await?;
    stream.write_all(&bytes).await?;
    stream.flush().await?;
    Ok(())
}

async fn receive_frame<T: serde::de::DeserializeOwned>(
    stream: &mut (impl AsyncRead + Unpin),
) -> Result<T> {
    let size = stream.read_u32().await? as usize;
    ensure!(size <= LIMIT, "member frame exceeds limit");
    let mut bytes = vec![0; size];
    stream.read_exact(&mut bytes).await?;
    Ok(serde_json::from_slice(&bytes)?)
}

fn unwrap_reply(generation: &str, reply: Reply) -> Result<Value> {
    ensure!(reply.generation == generation, "stale member reply");
    reply.outcome.map_err(anyhow::Error::msg)
}

fn blocking_call(address: SocketAddr, request: Request) -> Result<Value> {
    ensure!(
        address.ip().is_loopback(),
        "member endpoint is not loopback"
    );
    let mut stream = BlockingStream::connect_timeout(&address, RPC_TIMEOUT)?;
    stream.set_read_timeout(Some(RPC_TIMEOUT))?;
    stream.set_write_timeout(Some(RPC_TIMEOUT))?;
    write_frame(&mut stream, &request)?;
    unwrap_reply(&request.generation, read_frame(&mut stream)?)
}

async fn call(address: SocketAddr, request: Request) -> Result<Value> {
    tokio::time::timeout(RPC_TIMEOUT, async {
        ensure!(
            address.ip().is_loopback(),
            "member endpoint is not loopback"
        );
        let mut stream = TcpStream::connect(address).await?;
        send_frame(&mut stream, &request).await?;
        unwrap_reply(&request.generation, receive_frame(&mut stream).await?)
    })
    .await?
}

async fn call_data(address: SocketAddr, request: Request) -> Result<Value> {
    ensure!(
        address.ip().is_loopback(),
        "member endpoint is not loopback"
    );
    let mut stream = tokio::time::timeout(RPC_TIMEOUT, async {
        let mut stream = TcpStream::connect(address).await?;
        send_frame(&mut stream, &request).await?;
        Ok::<_, anyhow::Error>(stream)
    })
    .await??;
    // Preserve the application's existing observer/scenario budget. A native
    // quorum write can outlive a control-RPC timeout and a dropped observer;
    // its actual child owner still retains the admitted operation.
    unwrap_reply(&request.generation, receive_frame(&mut stream).await?)
}

#[derive(Clone)]
pub(crate) struct MemberClient {
    address: SocketAddr,
    lifecycle: SocketAddr,
    pub(super) generation: String,
    live: Arc<AtomicBool>,
    pid: u32,
    identity: Arc<Mutex<Option<PublicV1AgentReport>>>,
}

impl MemberClient {
    pub(super) fn validate_recovery_cause(&self, context: &ReplicaContext, pid: u32) -> Result<()> {
        ensure!(
            pid == self.pid && !self.live.load(Ordering::Acquire),
            "member recovery requires its exact reaped generation"
        );
        let report = self
            .identity
            .lock()
            .unwrap()
            .clone()
            .context("member identity was never observed")?;
        ensure!(
            report.replica == context.identity,
            "crossed recovery identity"
        );
        ReplicaHandle::validate_retained_recovery_exit_cause(
            context.root.parent().context("member data root missing")?,
            &report.history,
            &report.storage_incarnation,
            &report.process_session_id,
            &report.replicator_creation_id,
            pid,
        )?;
        Ok(())
    }

    fn request(&self, operation: &str, payload: Value) -> Result<Request> {
        ensure!(self.live.load(Ordering::Acquire), "member handle is stale");
        Ok(Request {
            generation: self.generation.clone(),
            operation: operation.to_owned(),
            payload,
        })
    }

    async fn request_async(&self, operation: &str, payload: Value) -> Result<Value> {
        let request = self.request(operation, payload)?;
        let reply = if operation == "data" {
            call_data(self.address, request).await?
        } else {
            call(self.address, request).await?
        };
        ensure!(self.live.load(Ordering::Acquire), "member handle is stale");
        Ok(reply)
    }

    pub(super) async fn data(&self, payload: Value) -> Result<Value> {
        self.request_async("data", payload).await
    }

    pub(super) async fn exit_gate_reply(&self, payload: Value) -> Result<Value> {
        call(self.address, self.request("data", payload)?).await
    }

    pub(super) fn observation(&self) -> Result<Value> {
        let reply = blocking_call(self.address, self.request("observation", Value::Null)?)?;
        ensure!(self.live.load(Ordering::Acquire), "member handle is stale");
        Ok(reply)
    }

    async fn lifecycle_request(&self, operation: &str) -> Result<Value> {
        let reply = call(self.lifecycle, self.request(operation, Value::Null)?).await?;
        ensure!(self.live.load(Ordering::Acquire), "member handle is stale");
        Ok(reply)
    }

    fn lifecycle_observation(&self, operation: &str) -> Result<Value> {
        let reply = blocking_call(self.lifecycle, self.request(operation, Value::Null)?)?;
        ensure!(self.live.load(Ordering::Acquire), "member handle is stale");
        Ok(reply)
    }
}

struct ChildState {
    child: Child,
    reaped: Option<ExitStatus>,
}

struct ChildOwner {
    state: Mutex<ChildState>,
    live: Arc<AtomicBool>,
}

impl ChildOwner {
    fn poll(&self) -> Result<Option<ExitStatus>> {
        let mut state = self.state.lock().unwrap();
        if state.reaped.is_none() {
            state.reaped = state.child.try_wait()?;
        }
        if state.reaped.is_some() {
            self.live.store(false, Ordering::Release);
        }
        Ok(state.reaped)
    }

    fn kill_and_reap(&self) -> Result<ExitStatus> {
        self.live.store(false, Ordering::Release);
        let mut state = self.state.lock().unwrap();
        if let Some(status) = state.reaped {
            return Ok(status);
        }
        // The root PID is still owned/unreaped, so its process-group ID cannot
        // have been recycled. Never signal a name or a remembered reaped PID.
        #[cfg(target_os = "linux")]
        unsafe {
            if kill(-(state.child.id() as i32), 9) != 0 {
                let error = std::io::Error::last_os_error();
                if error.raw_os_error() != Some(3) {
                    return Err(error.into());
                }
            }
        }
        #[cfg(not(target_os = "linux"))]
        state.child.kill()?;
        let status = state.child.wait()?;
        state.reaped = Some(status);
        Ok(status)
    }
}

impl Drop for ChildOwner {
    fn drop(&mut self) {
        // Drop is a physical crash, not a successful logical stop receipt.
        // Keep the exact Child until wait completes, including outside Tokio.
        while let Err(error) = self.kill_and_reap() {
            // Unknown exclusion must block, not lose the last exact Child owner.
            eprintln!("owned member could not be reaped; exclusion blocked: {error:#}");
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

/// Exact owned OS exit, not a logical Close result.
pub(crate) struct ReapedMember {
    pub(crate) pid: u32,
    pub(crate) status: ExitStatus,
}

pub(crate) struct ProcessMember {
    owner: Arc<ChildOwner>,
    pub(crate) context: ReplicaContext,
    pub(crate) control: String,
    pid: u32,
    router_shutdown: watch::Sender<bool>,
    router: Option<JoinHandle<Result<()>>>,
}

impl ProcessMember {
    pub(crate) async fn launch(
        executable: PathBuf,
        config: &ReplicaProcessConfig,
        mut context: ReplicaContext,
        api: ApiServer,
        network: Network,
        namespace: &str,
    ) -> Result<Self> {
        ensure!(
            executable.is_absolute() && executable.is_file(),
            "explicit member executable is missing: {}",
            executable.display()
        );
        #[cfg(not(target_os = "linux"))]
        bail!("physical test members require Linux parent-death containment");
        let generation = uuid::Uuid::new_v4().to_string();
        let router_listener = TcpListener::bind("127.0.0.1:0").await?;
        let bootstrap = Bootstrap {
            generation: generation.clone(),
            parent_pid: std::process::id(),
            identity: context.identity.clone(),
            resource_uid: config.resource_uid.clone(),
            pvc_uid: config.pvc_uid.clone(),
            data_root: config.data_root.clone(),
            environment: context.environment.clone(),
            bearer_token: config.bearer_token.clone(),
            router: router_listener.local_addr()?,
            namespace: namespace.to_owned(),
            transport_window_capacity: config.transport_window_capacity,
        };
        let mut command = Command::new(executable);
        command
            .arg("--kuberic-test-member")
            .env("KUBERIC_TEST_MEMBER_PARENT", std::process::id().to_string())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        #[cfg(target_os = "linux")]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        let mut child = command
            .spawn()
            .context("spawn explicit member executable")?;
        let pid = child.id();
        let mut input = child.stdin.take().context("member input unavailable")?;
        let mut output = child.stdout.take().context("member output unavailable")?;
        let live = Arc::new(AtomicBool::new(true));
        let owner = Arc::new(ChildOwner {
            state: Mutex::new(ChildState {
                child,
                reaped: None,
            }),
            live: live.clone(),
        });
        let (router_shutdown, shutdown) = watch::channel(false);
        let route_generation = generation.clone();
        let source = context.identity.instance_id.to_string();
        let router = tokio::spawn(serve_router(
            router_listener,
            shutdown,
            route_generation,
            source,
            api,
            network,
        ));
        let mut member = Self {
            owner,
            context: context.clone(),
            control: String::new(),
            pid,
            router_shutdown,
            router: Some(router),
        };
        let mut startup = tokio::task::spawn_blocking(move || {
            write_frame(&mut input, &bootstrap)?;
            // EOF is not a shutdown command. Parent death is a separate kernel fence.
            drop(input);
            read_frame::<Ready>(&mut output)
        });
        let ready = match tokio::time::timeout(RPC_TIMEOUT, &mut startup).await {
            Ok(result) => result
                .map_err(anyhow::Error::from)
                .and_then(|result| result),
            Err(error) => {
                member.owner.kill_and_reap()?;
                // Retain and join the blocking pipe reader after actual death.
                let _ = startup.await;
                member.finish_router().await?;
                return Err(error.into());
            }
        };
        let ready = ready.and_then(|ready| {
            ensure!(
                ready.pid == pid && ready.generation == generation,
                "crossed member startup"
            );
            ensure!(
                ready.data.ip().is_loopback(),
                "non-loopback member data endpoint"
            );
            ensure!(
                ready.lifecycle.ip().is_loopback(),
                "non-loopback member lifecycle endpoint"
            );
            Ok(ready)
        });
        let ready = match ready {
            Ok(ready) => ready,
            Err(error) => {
                member.owner.kill_and_reap()?;
                member.finish_router().await?;
                return Err(error);
            }
        };
        context.member = Some(MemberClient {
            address: ready.data,
            lifecycle: ready.lifecycle,
            generation,
            live,
            pid,
            identity: Arc::new(Mutex::new(None)),
        });
        member.context = context;
        member.control = ready.control;
        Ok(member)
    }

    pub(crate) fn live(&self) -> bool {
        self.owner.live.load(Ordering::Acquire)
    }

    pub(crate) fn poll_exit(&self) -> Result<Option<ReapedMember>> {
        Ok(self.owner.poll()?.map(|status| ReapedMember {
            pid: self.pid,
            status,
        }))
    }

    pub(crate) fn reaped_observation(&self, exit: &ReapedMember) -> Result<Value> {
        ensure!(
            self.owner.poll()? == Some(exit.status) && exit.pid == self.pid,
            "exit observation requires the exact reaped Child"
        );
        Ok(json!({
            "generation": self.client()?.generation,
            "pid": exit.pid,
            "success": exit.status.success(),
            "code": exit.status.code(),
            "instance": self.context.identity.instance_id,
        }))
    }

    pub(crate) async fn identity(&self) -> Result<(PublicV1AgentReport, ProcessSessionEndpoint)> {
        let value = self.client()?.lifecycle_request("identity").await?;
        let identity: (PublicV1AgentReport, ProcessSessionEndpoint) =
            serde_json::from_value(value)?;
        ensure!(
            identity.0.replica == self.context.identity
                && identity.0.process_session_id == identity.1.process_session_id
                && identity.0.replicator_creation_id == identity.1.replicator_creation_id
                && identity.0.storage_incarnation == identity.1.storage_incarnation,
            "crossed child identity"
        );
        *self.client()?.identity.lock().unwrap() = Some(identity.0.clone());
        Ok(identity)
    }

    fn client(&self) -> Result<&MemberClient> {
        self.context
            .member
            .as_ref()
            .context("member startup incomplete")
    }

    pub(crate) fn inspect_native(&self) -> Result<Value> {
        Ok(self.client()?.lifecycle_observation("native")?["native"].clone())
    }

    pub(crate) fn supports(
        &self,
        session: &ProcessSessionId,
        creation: &ReplicatorCreationId,
    ) -> Result<bool> {
        if !self.live() || self.poll_exit()?.is_some() {
            return Ok(false);
        }
        let observation = self.client()?.lifecycle_observation("attachment")?;
        ensure!(
            observation["pid"].as_u64() == Some(u64::from(self.pid)),
            "crossed child attachment"
        );
        let attached: Option<(ProcessSessionId, ReplicatorCreationId)> =
            serde_json::from_value(observation["attachment"].clone())?;
        Ok(self.poll_exit()?.is_none()
            && attached.as_ref() == Some(&(session.clone(), creation.clone())))
    }

    async fn finish_router(&mut self) -> Result<()> {
        self.router_shutdown.send_replace(true);
        if let Some(router) = self.router.take() {
            router.await??;
        }
        Ok(())
    }

    pub(crate) async fn finish_exited(mut self) -> Result<()> {
        ensure!(
            self.poll_exit()?.is_some(),
            "member has not actually exited"
        );
        self.finish_router().await
    }

    pub(crate) async fn stop(mut self) -> Result<()> {
        // A clean exit is accepted only after the child has actually joined the
        // host and application. Timeout/error still physically excludes it but
        // never earns the successful-stop receipt used by request_restart.
        let stopped = self.client()?.request_async("shutdown", Value::Null).await;
        self.owner.live.store(false, Ordering::Release);
        let owner = self.owner.clone();
        let reaped = tokio::task::spawn_blocking(move || owner.kill_and_reap()).await?;
        let router = self.finish_router().await;
        stopped?;
        reaped?;
        router?;
        Ok(())
    }
}

impl Drop for ProcessMember {
    fn drop(&mut self) {
        self.owner.live.store(false, Ordering::Release);
        // Physical exclusion precedes routing destruction or any successor.
        let _ = self.owner.kill_and_reap();
        self.router_shutdown.send_replace(true);
    }
}

fn route_kind(value: &Value) -> Result<TestingTcpRouteKind> {
    use TestingTcpRouteKind::*;
    Ok(match value.as_str().context("route kind missing")? {
        "Replication" => Replication,
        "Build" => Build,
        "EngineReserve" => EngineReserve,
        "EngineActivate" => EngineActivate,
        "EngineConfirm" => EngineConfirm,
        "EngineComplete" => EngineComplete,
        "EngineRetire" => EngineRetire,
        "ConfirmReceived" => ConfirmReceived,
        "BeforeDispatch" => BeforeDispatch,
        "BeforeCredit" => BeforeCredit,
        "AfterContext" => AfterContext,
        "BuildResult" => BuildResult,
        "BeforeDelivery" => BeforeDelivery,
        "RetireResponse" => RetireResponse,
        "RecoveryDiscovery" => RecoveryDiscovery,
        "RecoveryReceived" => RecoveryReceived,
        "ConfigurationValidated" => ConfigurationValidated,
        "ConfigurationAdmitted" => ConfigurationAdmitted,
        "RemoveAdmitted" => RemoveAdmitted,
        _ => bail!("unsupported native route kind"),
    })
}

async fn serve_router(
    listener: TcpListener,
    mut shutdown: watch::Receiver<bool>,
    generation: String,
    source: String,
    api: ApiServer,
    network: Network,
) -> Result<()> {
    let mut tasks = JoinSet::new();
    let outcome: Result<()> = async {
        loop {
            tokio::select! {
                biased;
                _ = shutdown.wait_for(|stopped| *stopped) => break,
                accepted = listener.accept(), if tasks.len() < REQUEST_CAPACITY => {
                    let (mut stream, _) = accepted?;
                    let generation = generation.clone();
                    let source = source.clone();
                    let api = api.clone();
                    let network = network.clone();
                    tasks.spawn(async move {
                        let request = tokio::time::timeout(
                            RPC_TIMEOUT, receive_frame::<Request>(&mut stream),
                        ).await??;
                        let result: Result<Value> = async {
                            ensure!(request.generation == generation, "stale child router");
                            match request.operation.as_str() {
                                "route" => {
                                    ensure!(request.payload.as_object().is_some_and(|p| p.len() == 3), "invalid route fields");
                                    let kind = route_kind(&request.payload["kind"])?;
                                    let destination = request.payload["destination"].as_str().context("route destination missing")?;
                                    let endpoint = request.payload["endpoint"].as_str().context("route endpoint missing")?;
                                    Ok(json!(network.route_data(kind, &source, destination, endpoint).await?))
                                }
                                "resolve" => {
                                    let destination = request.payload.as_str().context("resolve destination missing")?;
                                    let endpoint = api.snapshot(Kind::Pod).into_iter()
                                        .find(|pod| pod["metadata"]["uid"].as_str() == Some(destination))
                                        .and_then(|pod| pod["metadata"]["annotations"][CONTROL_ADDRESS_ANNOTATION].as_str().map(str::to_owned))
                                        .context("peer control endpoint unavailable")?;
                                    Ok(json!(endpoint))
                                }
                                _ => bail!("router accepts destinations only"),
                            }
                        }.await;
                        send_frame(&mut stream, &reply(generation, result)).await
                    });
                }
                joined = tasks.join_next(), if !tasks.is_empty() => {
                    if let Err(error) = joined.context("router task missing")?? {
                        eprintln!("member routing client closed/rejected: {error:#}");
                    }
                }
            }
        }
        Ok(())
    }.await;
    tasks.abort_all();
    while let Some(joined) = tasks.join_next().await {
        match joined {
            Ok(Ok(())) => {}
            Ok(Err(error)) => eprintln!("member routing client closed/rejected: {error:#}"),
            Err(error) if error.is_cancelled() => {}
            Err(error) => return Err(error.into()),
        }
    }
    outcome
}

fn reply(generation: String, result: Result<Value>) -> Reply {
    match result {
        Ok(value) => Reply {
            generation,
            outcome: Ok(value),
        },
        Err(error) => Reply {
            generation,
            outcome: Err(format!("{error:#}")),
        },
    }
}

struct ChildResolver {
    router: SocketAddr,
    generation: String,
    dns: KubernetesDnsResolver,
}

impl ReplicaEndpointResolver for ChildResolver {
    fn control_endpoint(&self, identity: &ReplicaIdentity) -> String {
        blocking_call(
            self.router,
            Request {
                generation: self.generation.clone(),
                operation: "resolve".into(),
                payload: json!(identity.instance_id.as_str()),
            },
        )
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_else(|| "http://127.0.0.1:0".into())
    }
    fn replication_endpoint(&self, identity: &ReplicaIdentity) -> String {
        self.dns.replication_endpoint(identity)
    }
}

#[cfg(target_os = "linux")]
unsafe extern "C" {
    fn prctl(option: i32, ...) -> i32;
    fn getppid() -> i32;
    fn kill(pid: i32, signal: i32) -> i32;
}

pub(super) fn member_main<F: ApplicationFactory>(factory: F) -> Result<()> {
    #[cfg(not(target_os = "linux"))]
    bail!("test member parent-death containment requires Linux");
    #[cfg(target_os = "linux")]
    {
        let expected: u32 = std::env::var("KUBERIC_TEST_MEMBER_PARENT")?.parse()?;
        // Set before creating Tokio threads, then close the fork/exec/death race.
        unsafe {
            ensure!(
                prctl(1, 9, 0, 0, 0) == 0,
                "cannot arm member parent-death fence"
            );
            ensure!(getppid() as u32 == expected, "member parent already gone");
        }
        let bootstrap: Bootstrap = read_frame(&mut std::io::stdin().lock())?;
        ensure!(bootstrap.parent_pid == expected, "crossed member parent");
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()?;
        let result = runtime.block_on(run_member(factory, bootstrap));
        if result
            .as_ref()
            .err()
            .is_some_and(|error| error.is::<RetainedMemberExit>())
        {
            // Only the explicit member executable owns process exit. Do not
            // wait here on workers whose failed drainage caused this notice.
            // No physical exclusion is claimed until its supervisor reaps it.
            runtime.shutdown_background();
        }
        result
    }
}

#[derive(Debug, thiserror::Error)]
#[error("durable retained-member recovery requires physical executable exit")]
struct RetainedMemberExit;

async fn run_member<F: ApplicationFactory>(factory: F, bootstrap: Bootstrap) -> Result<()> {
    let factory = Arc::new(factory);
    let context = ReplicaContext {
        identity: bootstrap.identity.clone(),
        root: bootstrap.data_root.join("application"),
        environment: bootstrap.environment,
        member: None,
    };
    let application = factory.create(context.clone()).await?;
    application.validate(&context.root)?;
    let service = application.service;
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let control = format!("http://{}", listener.local_addr()?);
    let data = TcpListener::bind("127.0.0.1:0").await?;
    let data_address = data.local_addr()?;
    let lifecycle = TcpListener::bind("127.0.0.1:0").await?;
    let lifecycle_address = lifecycle.local_addr()?;
    let router = bootstrap.router;
    let generation = bootstrap.generation.clone();
    let route_generation = generation.clone();
    let route = Arc::new(TestingTcpRoute::register_with_kind(
        bootstrap.identity.instance_id.to_string(),
        move |kind, destination, endpoint| {
            let generation = route_generation.clone();
            Box::pin(async move {
                let value = call(router, Request {
                    generation,
                    operation: "route".into(),
                    payload: json!({"kind":format!("{kind:?}"),"destination":destination,"endpoint":endpoint}),
                }).await.map_err(std::io::Error::other)?;
                value
                    .as_str()
                    .map(str::to_owned)
                    .context("invalid route destination")
                    .map_err(std::io::Error::other)
            })
        },
    )?);
    let resolver = Arc::new(ChildResolver {
        router,
        generation: generation.clone(),
        dns: KubernetesDnsResolver::new(bootstrap.resource_uid.clone(), bootstrap.namespace),
    });
    let config = ReplicaProcessConfig {
        resource_uid: bootstrap.resource_uid,
        replica_id: ReplicaId::new(bootstrap.identity.replica_id.value()),
        pod_uid: PodUid::new(bootstrap.identity.instance_id.as_str()),
        pvc_uid: bootstrap.pvc_uid,
        data_root: bootstrap.data_root,
        replication_address: "127.0.0.1:0".parse()?,
        bearer_token: bootstrap.bearer_token,
        rpc_deadline: RPC_TIMEOUT,
        transport_window_capacity: bootstrap.transport_window_capacity,
    };
    let mut host = ReplicaHost::new(config, service.clone(), application.storage, resolver);
    if !application.paths.is_empty() {
        host = host.with_application_storage_paths(
            application
                .paths
                .into_iter()
                .enumerate()
                .map(|(index, path)| (format!("application-{index}"), path))
                .collect(),
        );
    }
    let (startup_shutdown, receiver) = watch::channel(false);
    let mut startup = tokio::spawn(host.start_with_shutdown(listener, receiver));
    let (handle, observed_handle) = watch::channel(None::<ReplicaHandle>);
    let (lifecycle_shutdown, lifecycle_stopping) = watch::channel(false);
    let mut lifecycle_server = tokio::spawn(serve_member_lifecycle(
        lifecycle,
        generation.clone(),
        route,
        observed_handle,
        lifecycle_stopping,
    ));
    let (stop, mut stopping) = watch::channel(false);
    let (settled, observed_settled) = watch::channel(None::<std::result::Result<(), String>>);
    let mut server = tokio::spawn(serve_member_data(
        data,
        generation.clone(),
        factory.clone(),
        service.clone(),
        stop,
        observed_settled,
    ));
    // Publishing sockets does not assert initialization, attachment or readiness.
    write_frame(
        &mut std::io::stdout().lock(),
        &Ready {
            generation,
            pid: std::process::id(),
            control,
            data: data_address,
            lifecycle: lifecycle_address,
        },
    )?;
    let mut running = tokio::select! {
        result = &mut startup => result??,
        result = &mut server => {
            result??;
            bail!("member data server stopped before host startup");
        }
        result = &mut lifecycle_server => {
            result??;
            bail!("member lifecycle server stopped before host startup");
        }
        _ = stopping.wait_for(|stopped| *stopped) => {
            startup_shutdown.send_replace(true);
            startup.await??
        }
    };
    if let Some(running) = &running {
        handle.send_replace(Some(running.handle()));
    }
    if !*stopping.borrow()
        && let Some(running) = running.as_mut()
    {
        let recovery = running.handle();
        tokio::select! {
            result = recovery.wait_for_retained_recovery_exit() => {
                factory.before_member_recovery_exit(service.clone(), result?).await?;
                return Err(RetainedMemberExit.into());
            }
            _ = stopping.wait_for(|stopped| *stopped) => {}
            result = running.wait() => {
                // Unexpected host completion is not a recoverable OS exit.
                result?;
                bail!("member host stopped unexpectedly");
            }
            result = &mut server => {
                result??;
                bail!("member data server stopped unexpectedly");
            }
            result = &mut lifecycle_server => {
                result??;
                bail!("member lifecycle server stopped unexpectedly");
            }
        }
    }
    let mut result: Result<()> = async {
        if let Some(mut running) = running {
            running.shutdown();
            running.wait().await?;
        }
        factory.shutdown_and_drain(service).await?;
        Ok(())
    }
    .await;
    lifecycle_shutdown.send_replace(true);
    if let Err(error) = lifecycle_server
        .await
        .context("member lifecycle join")
        .and_then(|result| result)
    {
        result = Err(match result {
            Ok(()) => error,
            Err(previous) => previous.context(format!("lifecycle cleanup: {error:#}")),
        });
    }
    settled.send_replace(Some(
        result
            .as_ref()
            .map(|_| ())
            .map_err(|error| format!("{error:#}")),
    ));
    server.await??;
    result
}

async fn serve_member_data<F: ApplicationFactory>(
    listener: TcpListener,
    generation: String,
    factory: Arc<F>,
    service: Arc<F::Service>,
    stop: watch::Sender<bool>,
    settled: watch::Receiver<Option<std::result::Result<(), String>>>,
) -> Result<()> {
    let mut tasks = JoinSet::new();
    loop {
        tokio::select! {
            accepted = listener.accept(), if tasks.len() < REQUEST_CAPACITY => {
                let (mut stream, _) = accepted?;
                let generation = generation.clone();
                let factory = factory.clone();
                let service = service.clone();
                let stop = stop.clone();
                let mut settled = settled.clone();
                tasks.spawn(async move {
                    let request: Request = tokio::time::timeout(
                        RPC_TIMEOUT, receive_frame(&mut stream),
                    ).await??;
                    let shutdown = request.operation == "shutdown" && request.generation == generation;
                    let result: Result<Value> = async {
                        ensure!(request.generation == generation, "stale member request");
                        match request.operation.as_str() {
                            "data" => factory.member_data(service.clone(), request.payload.clone()).await.map_err(Into::into),
                            "observation" => Ok(json!({
                                "pid":std::process::id(),
                                "application":factory.member_observation(&service)?,
                            })),
                            "shutdown" => {
                                stop.send_replace(true);
                                let result = settled.wait_for(|result| result.is_some()).await?.clone().context("member shutdown outcome missing")?;
                                result.map_err(anyhow::Error::msg)?;
                                Ok(Value::Null)
                            }
                            _ => bail!("unsupported member data operation"),
                        }
                    }.await;
                    let successful = result.is_ok();
                    send_frame(&mut stream, &reply(generation, result)).await?;
                    if successful && request.operation == "data" {
                        factory.member_data_reply_sent(&service, &request.payload);
                    }
                    Ok::<_, anyhow::Error>(shutdown)
                });
            }
            joined = tasks.join_next(), if !tasks.is_empty() => {
                match joined.context("member task missing")?? {
                    Ok(true) => {
                        tasks.abort_all();
                        break;
                    }
                    Ok(false) => {}
                    Err(error) => eprintln!("member data client closed/rejected: {error:#}"),
                }
            }
        }
    }
    while let Some(joined) = tasks.join_next().await {
        match joined {
            Ok(Ok(_)) => {}
            Ok(Err(error)) => eprintln!("member data client closed/rejected: {error:#}"),
            Err(error) if error.is_cancelled() => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

async fn serve_member_lifecycle(
    listener: TcpListener,
    generation: String,
    route: Arc<TestingTcpRoute>,
    handle: watch::Receiver<Option<ReplicaHandle>>,
    mut stopping: watch::Receiver<bool>,
) -> Result<()> {
    let mut tasks = JoinSet::new();
    let outcome: Result<()> = async {
        loop {
            tokio::select! {
                biased;
                _ = stopping.wait_for(|stopped| *stopped) => break,
                accepted = listener.accept(), if tasks.len() < REQUEST_CAPACITY => {
                    let (mut stream, _) = accepted?;
                    let generation = generation.clone();
                    let route = route.clone();
                    let handle = handle.clone();
                    tasks.spawn(async move {
                        let request: Request = tokio::time::timeout(
                            RPC_TIMEOUT, receive_frame(&mut stream),
                        ).await??;
                        let result: Result<Value> = async {
                            ensure!(request.generation == generation, "stale lifecycle observation");
                            ensure!(request.payload.is_null(), "lifecycle observations have no input authority");
                            match request.operation.as_str() {
                                "identity" => {
                                    let handle = handle.borrow().clone().context("member initialization pending")?;
                                    Ok(serde_json::to_value((
                                        handle.public_v1_report().await?,
                                        handle.public_v1_process_endpoint(ReplicaRole::None).await?,
                                    ))?)
                                }
                                "attachment" => Ok(json!({
                                    "pid":std::process::id(),
                                    "attachment":route.attachment(),
                                })),
                                "native" => Ok(json!({
                                    "pid":std::process::id(),
                                    "native":route.inspect_native().unwrap_or_else(|error| json!({"unavailable":error.to_string()})),
                                })),
                                _ => bail!("lifecycle stream is read-only"),
                            }
                        }.await;
                        send_frame(&mut stream, &reply(generation, result)).await
                    });
                }
                joined = tasks.join_next(), if !tasks.is_empty() => {
                    if let Err(error) = joined.context("lifecycle task missing")?? {
                        eprintln!("member lifecycle client closed/rejected: {error:#}");
                    }
                }
            }
        }
        Ok(())
    }.await;
    tasks.abort_all();
    while let Some(joined) = tasks.join_next().await {
        match joined {
            Ok(Ok(())) => {}
            Ok(Err(error)) => eprintln!("member lifecycle client closed/rejected: {error:#}"),
            Err(error) if error.is_cancelled() => {}
            Err(error) => return Err(error.into()),
        }
    }
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[tokio::test]
    async fn native_data_observer_keeps_its_caller_budget_beyond_control_rpc_timeout() -> Result<()>
    {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let entered = Arc::new(tokio::sync::Semaphore::new(0));
        let release = Arc::new(tokio::sync::Notify::new());
        let server = tokio::spawn({
            let entered = entered.clone();
            let release = release.clone();
            async move {
                let (mut stream, _) = listener.accept().await?;
                let request: Request = receive_frame(&mut stream).await?;
                entered.add_permits(1);
                release.notified().await;
                send_frame(
                    &mut stream,
                    &Reply {
                        generation: request.generation,
                        outcome: Ok(json!({"acknowledged": true})),
                    },
                )
                .await
            }
        });
        let observer = tokio::spawn(call_data(
            address,
            Request {
                generation: "owned-observer".into(),
                operation: "data".into(),
                payload: Value::Null,
            },
        ));
        entered.acquire().await?.forget();
        tokio::time::sleep(RPC_TIMEOUT + Duration::from_millis(100)).await;
        ensure!(
            !observer.is_finished(),
            "control timeout truncated the native data observer"
        );
        release.notify_one();
        ensure!(tokio::time::timeout(RPC_TIMEOUT, observer).await???["acknowledged"] == true);
        server.await??;
        Ok(())
    }

    #[test]
    fn framed_null_success_and_typed_errors_are_not_conflated() -> Result<()> {
        for outcome in [
            Ok(Value::Null),
            Ok(json!(41)),
            Err("real Close failed".into()),
        ] {
            let mut bytes = Vec::new();
            write_frame(
                &mut bytes,
                &Reply {
                    generation: "exact-generation".into(),
                    outcome: outcome.clone(),
                },
            )?;
            let decoded: Reply = read_frame(&mut Cursor::new(bytes))?;
            let observed = unwrap_reply("exact-generation", decoded);
            match outcome {
                Ok(value) => assert_eq!(observed?, value),
                Err(error) => assert_eq!(observed.unwrap_err().to_string(), error),
            }
        }
        Ok(())
    }

    #[test]
    fn oversized_frames_crossed_generations_and_protocol_route_fields_refuse() {
        let bytes = ((LIMIT + 1) as u32).to_be_bytes();
        assert!(read_frame::<Request>(&mut Cursor::new(bytes)).is_err());
        assert!(
            unwrap_reply(
                "new",
                Reply {
                    generation: "old".into(),
                    outcome: Ok(json!({"attachment":"not authority"})),
                }
            )
            .is_err()
        );
        assert!(route_kind(&json!("ReplicaGoal")).is_err());
        assert!(route_kind(&json!("Ready")).is_err());
        assert!(route_kind(&json!("Build")).is_ok());
    }

    #[tokio::test]
    async fn stale_proxy_rejects_before_dial_for_data_and_read_only_observation() {
        let client = MemberClient {
            address: "127.0.0.1:0".parse().unwrap(),
            lifecycle: "127.0.0.1:0".parse().unwrap(),
            generation: "old-owned-child".into(),
            live: Arc::new(AtomicBool::new(false)),
            pid: 0,
            identity: Arc::new(Mutex::new(None)),
        };
        assert!(
            client
                .data(json!({"set": 99}))
                .await
                .unwrap_err()
                .to_string()
                .contains("stale")
        );
        assert!(
            client
                .observation()
                .unwrap_err()
                .to_string()
                .contains("stale")
        );
    }

    #[tokio::test]
    async fn lifecycle_stream_requires_actual_attachment_and_rejects_data_and_stale_inputs()
    -> Result<()> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let route = Arc::new(TestingTcpRoute::register(
            uuid::Uuid::new_v4().to_string(),
            |_, endpoint| Box::pin(async { Ok(endpoint) }),
        )?);
        let (_handle, observation) = watch::channel(None::<ReplicaHandle>);
        let (stop, stopping) = watch::channel(false);
        let server = tokio::spawn(serve_member_lifecycle(
            listener,
            "owned-generation".into(),
            route,
            observation,
            stopping,
        ));
        for (generation, operation, payload) in [
            ("old-generation", "attachment", Value::Null),
            ("owned-generation", "data", json!({"Set": 999})),
            ("owned-generation", "attachment", json!({"Ready": true})),
        ] {
            assert!(
                call(
                    address,
                    Request {
                        generation: generation.into(),
                        operation: operation.into(),
                        payload,
                    }
                )
                .await
                .is_err()
            );
        }
        let observation = call(
            address,
            Request {
                generation: "owned-generation".into(),
                operation: "attachment".into(),
                payload: Value::Null,
            },
        )
        .await?;
        assert!(
            observation["attachment"].is_null(),
            "a route callback is not native attachment"
        );
        stop.send_replace(true);
        server.await??;
        Ok(())
    }

    #[cfg(target_os = "linux")]
    unsafe extern "C" {
        fn waitpid(pid: i32, status: *mut i32, options: i32) -> i32;
    }

    #[cfg(target_os = "linux")]
    fn owned_sleep() -> Result<(ChildOwner, u32)> {
        use std::os::unix::process::CommandExt;
        let child = Command::new("/bin/sleep")
            .arg("60")
            .process_group(0)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .spawn()?;
        let pid = child.id();
        Ok((
            ChildOwner {
                state: Mutex::new(ChildState {
                    child,
                    reaped: None,
                }),
                live: Arc::new(AtomicBool::new(true)),
            },
            pid,
        ))
    }

    #[cfg(target_os = "linux")]
    fn assert_already_reaped(pid: u32) {
        let mut status = 0;
        // ECHILD is observed for this exact Child, not inferred from task return,
        // a new UUID, an absent Pod, or a process-name lookup.
        assert_eq!(unsafe { waitpid(pid as i32, &mut status, 1) }, -1);
        assert_eq!(std::io::Error::last_os_error().raw_os_error(), Some(10));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn physical_exclusion_reaps_the_exact_child_and_invalidates_its_proxy_generation() -> Result<()>
    {
        use std::os::unix::process::ExitStatusExt;
        let (owner, pid) = owned_sleep()?;
        assert!(owner.poll()?.is_none());
        let status = owner.kill_and_reap()?;
        assert_eq!(status.signal(), Some(9));
        assert!(!owner.live.load(Ordering::Acquire));
        assert_eq!(owner.poll()?, Some(status));
        assert_eq!(owner.kill_and_reap()?, status);
        assert_already_reaped(pid);
        Ok(())
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn lost_async_observer_still_physically_kills_and_reaps_its_owned_child() -> Result<()> {
        let (owner, pid) = owned_sleep()?;
        let live = owner.live.clone();
        drop(owner);
        assert!(!live.load(Ordering::Acquire));
        assert_already_reaped(pid);
        Ok(())
    }
}
