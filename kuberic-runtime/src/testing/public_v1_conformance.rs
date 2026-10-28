use std::collections::BTreeSet;
#[cfg(unix)]
use std::io::{self, Read};
#[cfg(unix)]
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::process::Stdio;
use std::process::{Command, ExitStatus};
use std::time::{Duration, Instant};

// Start shared work at partition entry and leave process cleanup plus
// startup/archive-owner cleanup inside the unchanged outer test timeout.
const CONFORMANCE_OUTER_TIMEOUT: Duration = Duration::from_secs(270);
const CONFORMANCE_TIMEOUT: Duration = Duration::from_secs(240);
const CONFORMANCE_CLEANUP_TIMEOUT: Duration = Duration::from_secs(10);
const CONFORMANCE_OWNER_MARGIN: Duration = Duration::from_secs(20);
#[cfg(unix)]
const PROCESS_POLL_INTERVAL: Duration = Duration::from_millis(10);

#[derive(Clone, Copy)]
struct CommandDeadline {
    work: Instant,
    cleanup: Instant,
}

impl CommandDeadline {
    fn new(started: Instant, work: Duration, cleanup: Duration) -> Self {
        let work = started + work;
        Self {
            work,
            cleanup: work + cleanup,
        }
    }
}

fn partition_deadline(started: Instant) -> CommandDeadline {
    assert_eq!(
        CONFORMANCE_TIMEOUT + CONFORMANCE_CLEANUP_TIMEOUT + CONFORMANCE_OWNER_MARGIN,
        CONFORMANCE_OUTER_TIMEOUT,
    );
    CommandDeadline::new(started, CONFORMANCE_TIMEOUT, CONFORMANCE_CLEANUP_TIMEOUT)
}

#[derive(Debug)]
struct BoundedOutput {
    status: ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

#[cfg(unix)]
struct ProcessGroupGuard {
    child: std::process::Child,
    cleanup_deadline: Instant,
    terminated: bool,
    reaped: bool,
}

#[cfg(unix)]
impl ProcessGroupGuard {
    fn terminate(&mut self) {
        if !self.terminated {
            self.cleanup_deadline = self
                .cleanup_deadline
                .min(Instant::now() + CONFORMANCE_CLEANUP_TIMEOUT);
            terminate_process_tree(&mut self.child, self.cleanup_deadline);
            self.terminated = true;
        }
    }

    fn try_reap(&mut self) -> io::Result<Option<ExitStatus>> {
        let status = self.child.try_wait()?;
        self.reaped |= status.is_some();
        Ok(status)
    }
}

#[cfg(unix)]
impl Drop for ProcessGroupGuard {
    fn drop(&mut self) {
        if !self.reaped {
            self.terminate();
            while Instant::now() < self.cleanup_deadline {
                if !matches!(self.try_reap(), Ok(None)) {
                    return;
                }
                std::thread::sleep(
                    PROCESS_POLL_INTERVAL.min(
                        self.cleanup_deadline
                            .saturating_duration_since(Instant::now()),
                    ),
                );
            }
            let _ = self.try_reap();
        }
    }
}

#[cfg(unix)]
struct CapturedPipe<R> {
    pipe: Option<R>,
    bytes: Vec<u8>,
}

#[cfg(unix)]
impl<R: Read + AsRawFd> CapturedPipe<R> {
    fn new(pipe: R) -> Self {
        Self {
            pipe: Some(pipe),
            bytes: Vec::new(),
        }
    }

    fn read_ready(&mut self) -> io::Result<()> {
        if let Some(pipe) = &mut self.pipe {
            let mut bytes = [0; 8192];
            match pipe.read(&mut bytes) {
                Ok(0) => self.pipe = None,
                Ok(count) => self.bytes.extend_from_slice(&bytes[..count]),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }
}

#[cfg(unix)]
fn poll_output<O: Read + AsRawFd, E: Read + AsRawFd>(
    stdout: &mut CapturedPipe<O>,
    stderr: &mut CapturedPipe<E>,
    deadline: Instant,
) -> io::Result<()> {
    #[repr(C)]
    struct PollFd {
        fd: i32,
        events: i16,
        revents: i16,
    }
    #[cfg(any(
        target_os = "linux",
        target_os = "android",
        target_os = "illumos",
        target_os = "solaris"
    ))]
    type PollCount = std::os::raw::c_ulong;
    #[cfg(not(any(
        target_os = "linux",
        target_os = "android",
        target_os = "illumos",
        target_os = "solaris"
    )))]
    type PollCount = std::os::raw::c_uint;
    unsafe extern "C" {
        fn poll(fds: *mut PollFd, count: PollCount, timeout: i32) -> i32;
    }
    const INPUT: i16 = 1;
    const ERROR: i16 = 8;
    const HANGUP: i16 = 16;
    const INVALID: i16 = 32;
    let mut fds = [
        PollFd {
            fd: stdout.pipe.as_ref().map_or(-1, AsRawFd::as_raw_fd),
            events: INPUT,
            revents: 0,
        },
        PollFd {
            fd: stderr.pipe.as_ref().map_or(-1, AsRawFd::as_raw_fd),
            events: INPUT,
            revents: 0,
        },
    ];
    let timeout = deadline
        .saturating_duration_since(Instant::now())
        .min(PROCESS_POLL_INTERVAL)
        .as_millis() as i32;
    // This owner is the only reader, so a ready pipe read cannot lose its data
    // to another reader and block after poll. No reader thread outlives the budget.
    let result = unsafe { poll(fds.as_mut_ptr(), fds.len() as PollCount, timeout) };
    if result < 0 {
        let error = io::Error::last_os_error();
        return if error.kind() == io::ErrorKind::Interrupted {
            Ok(())
        } else {
            Err(error)
        };
    }
    if fds.iter().any(|fd| fd.revents & INVALID != 0) {
        return Err(io::Error::other("captured pipe descriptor became invalid"));
    }
    if fds[0].revents & (INPUT | ERROR | HANGUP) != 0 {
        stdout.read_ready()?;
    }
    if fds[1].revents & (INPUT | ERROR | HANGUP) != 0 {
        stderr.read_ready()?;
    }
    Ok(())
}

#[cfg(unix)]
fn cleanup_command<O: Read + AsRawFd, E: Read + AsRawFd>(
    owner: &mut ProcessGroupGuard,
    stdout: &mut CapturedPipe<O>,
    stderr: &mut CapturedPipe<E>,
) -> Result<ExitStatus, String> {
    owner.terminate();
    let mut status = None;
    let mut pipe_error = None;
    loop {
        if status.is_none() {
            status = owner
                .try_reap()
                .map_err(|error| format!("child reap failed: {error}"))?;
        }
        if stdout.pipe.is_none()
            && stderr.pipe.is_none()
            && let Some(status) = status
        {
            return match pipe_error {
                Some(error) => Err(format!("pipe drain failed: {error}")),
                None => Ok(status),
            };
        }
        if Instant::now() >= owner.cleanup_deadline {
            return Err(format!(
                "cleanup deadline exhausted: child reaped={}, undrained stdout={}, undrained stderr={}",
                status.is_some(),
                stdout.pipe.is_some(),
                stderr.pipe.is_some(),
            ));
        }
        if let Err(error) = poll_output(stdout, stderr, owner.cleanup_deadline) {
            pipe_error = Some(error);
            stdout.pipe = None;
            stderr.pipe = None;
        }
    }
}

#[cfg(unix)]
fn bounded_output(
    command: &mut Command,
    deadline: CommandDeadline,
    description: &str,
) -> Result<BoundedOutput, String> {
    if Instant::now() >= deadline.work {
        return Err(format!(
            "{description} exhausted the shared work deadline before spawn"
        ));
    }
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    use std::os::unix::process::CommandExt;
    command.process_group(0);
    let child = command
        .spawn()
        .map_err(|error| format!("{description} failed to start: {error}"))?;
    let mut owner = ProcessGroupGuard {
        child,
        cleanup_deadline: deadline.cleanup,
        terminated: false,
        reaped: false,
    };
    let stdout = owner
        .child
        .stdout
        .take()
        .ok_or_else(|| format!("{description} did not capture stdout"))?;
    let stderr = owner
        .child
        .stderr
        .take()
        .ok_or_else(|| format!("{description} did not capture stderr"))?;
    let mut stdout = CapturedPipe::new(stdout);
    let mut stderr = CapturedPipe::new(stderr);
    let failure = loop {
        if Instant::now() >= deadline.work {
            break "exceeded the shared work deadline".to_owned();
        }
        // Retain the unreaped leader while either pipe is open, pinning its
        // process-group identity even when it exits before its descendants.
        if stdout.pipe.is_none() && stderr.pipe.is_none() {
            match owner.try_reap() {
                Ok(Some(status)) => {
                    return Ok(BoundedOutput {
                        status,
                        stdout: stdout.bytes,
                        stderr: stderr.bytes,
                    });
                }
                Ok(None) => {}
                Err(error) => break format!("child status failed: {error}"),
            }
        }
        if let Err(error) = poll_output(&mut stdout, &mut stderr, deadline.work) {
            break format!("pipe read failed: {error}");
        }
    };
    let cleanup = match cleanup_command(&mut owner, &mut stdout, &mut stderr) {
        Ok(status) => format!("child reaped: {status}; captured pipes drained"),
        Err(error) => error,
    };
    Err(format!(
        "{description} {failure}; cleanup: {cleanup}\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&stdout.bytes),
        String::from_utf8_lossy(&stderr.bytes),
    ))
}

#[cfg(not(unix))]
fn bounded_output(
    _: &mut Command,
    _: CommandDeadline,
    description: &str,
) -> Result<BoundedOutput, String> {
    Err(format!(
        "{description} requires Unix process groups and bounded pipe polling"
    ))
}

#[cfg(unix)]
fn terminate_process_tree(child: &mut std::process::Child, deadline: Instant) {
    #[cfg(unix)]
    {
        unsafe extern "C" {
            fn kill(pid: i32, signal: i32) -> i32;
        }
        const SIGTERM: i32 = 15;
        const SIGKILL: i32 = 9;
        let process_group = -(child.id() as i32);
        unsafe {
            kill(process_group, SIGTERM);
        }
        let grace = (Instant::now() + Duration::from_millis(100)).min(deadline);
        while Instant::now() < grace {
            std::thread::sleep(
                Duration::from_millis(5).min(grace.saturating_duration_since(Instant::now())),
            );
        }
        unsafe {
            kill(process_group, SIGKILL);
        }
    }
    #[cfg(not(unix))]
    {
        let _ = child.kill();
    }
}

#[derive(Clone, Copy)]
struct Gate {
    id: &'static str,
    name: &'static str,
    owner: &'static str,
    postgres_scenarios: &'static [&'static str],
    replacement_scenarios: &'static [&'static str],
    expected: &'static str,
    eligibility: fn(),
}

const PUBLIC_LIFECYCLE: &str = include_str!("../host/public_lifecycle.rs");
const REPLICATOR: &str = include_str!("../replicator/mod.rs");
const NATIVE_MODULE_SOURCE: &str = include_str!("../replicator/v1/mod.rs");
const SERVICE_ADAPTER_SOURCE: &str = include_str!("../replicator/v1/service_adapter.rs");
const NATIVE_PRODUCTION_SOURCE: &str =
    include_str!("../replicator/v1/service_adapter/production.rs");
const HOST_PROCESS: &str = include_str!("../host/process.rs");
const PUBLIC_OPERATIONS: &str = include_str!("../protocol/public_operations.rs");
const HOST_OPERATION: &str = include_str!("../host/operation.rs");
const HOST_LIFECYCLE_PROGRAM: &str = include_str!("../host/public_lifecycle_program.rs");
const CONTROL_SERVICE_SOURCE: &str = include_str!("../host/service.rs");
const DATA_SERVICE_SOURCE: &str =
    include_str!("../replicator/v1/service_adapter/production_transport.rs");
const CONTROLLER_STARTUP_SOURCE: &str = include_str!("../../../kuberic-controller/src/lib.rs");
const CONTROL_SERVICE_WITH_UNMANIFESTED_ENDPOINT: &str = concat!(
    include_str!("../host/service.rs"),
    r#"
pub(crate) fn start_alternate_control_endpoint(service: AlternateContinuationService) {
    tonic::transport::Server::builder()
        .add_service(AlternateContinuationServer::new(service));
}
"#
);
const CONTROLLER_SOURCE_WITH_UNMANIFESTED_HELPER: &str = concat!(
    include_str!("../../../kuberic-controller/src/lib.rs"),
    r#"
pub(crate) fn start_alternate_controller_helper(
    sets: Api<crd::KubericSet>,
    alternate: Api<AlternateOwner>,
) {
    Controller::new(sets, watcher::Config::default())
        .owns(alternate, watcher::Config::default());
}
"#
);
#[allow(dead_code)]
mod controller_registration {
    include!("../../../kuberic-controller/src/public_v1_registration.rs");
}

fn contract_gate() {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplacementOwnershipInventory {
    pub factories: Vec<String>,
    pub services: Vec<String>,
    pub host_attachments: Vec<String>,
    pub report_branches: Vec<String>,
    pub store_tables: Vec<String>,
    pub startup_owners: Vec<String>,
    pub protocol_versions: Vec<String>,
    pub controller_modules: Vec<String>,
}

struct ProductionSources<'a> {
    public_operations: &'a str,
    host_operation: &'a str,
    host_lifecycle_program: &'a str,
    controller_manifest: controller_registration::PublicV1ControllerManifest,
    control_service_manifest: crate::host::service::PublicV1ControlServiceManifest,
    data_service_manifest: crate::replicator::v1::PublicV1DataServiceManifest,
    factory_registrations: Vec<crate::replicator::PublicV1FactoryRegistration>,
    control_service_source: &'a str,
    data_service_source: &'a str,
    factory_source: &'a str,
    native_module_source: &'a str,
    service_adapter_source: &'a str,
    native_production_source: &'a str,
    controller_startup_source: &'a str,
    host_attachments: &'a [&'a str],
    report_branches: &'a [&'a str],
    store_tables: &'a [&'a str],
    service_startup_owners: &'a [&'a str],
    process_startup_owners: &'a [&'a str],
    protocol_versions: &'a [&'a str],
}

fn production_sources() -> ProductionSources<'static> {
    ProductionSources {
        public_operations: PUBLIC_OPERATIONS,
        host_operation: HOST_OPERATION,
        host_lifecycle_program: HOST_LIFECYCLE_PROGRAM,
        controller_manifest: controller_registration::PUBLIC_V1_CONTROLLER,
        control_service_manifest: crate::host::service::PUBLIC_V1_CONTROL_SERVICES,
        data_service_manifest: crate::replicator::v1::PUBLIC_V1_DATA_SERVICES,
        factory_registrations: crate::replicator::PUBLIC_V1_FACTORIES
            .registrations()
            .to_vec(),
        control_service_source: CONTROL_SERVICE_SOURCE,
        data_service_source: DATA_SERVICE_SOURCE,
        factory_source: REPLICATOR,
        native_module_source: NATIVE_MODULE_SOURCE,
        service_adapter_source: SERVICE_ADAPTER_SOURCE,
        native_production_source: NATIVE_PRODUCTION_SOURCE,
        controller_startup_source: CONTROLLER_STARTUP_SOURCE,
        host_attachments: crate::host::hosting::PUBLIC_V1_HOST_ATTACHMENTS,
        report_branches: crate::host::service::PUBLIC_V1_AGENT_REPORT_BRANCHES,
        store_tables: crate::host::sqlite_store::PUBLIC_V1_AGENT_TABLES,
        service_startup_owners: crate::host::service::PUBLIC_V1_SERVICE_STARTUP_OWNERS,
        process_startup_owners: crate::host::PUBLIC_V1_PROCESS_STARTUP_OWNERS,
        protocol_versions: crate::protocol::public_operations::PUBLIC_V1_PROTOCOL_REGISTRATIONS,
    }
}

fn sorted_unique(mut values: Vec<String>) -> Vec<String> {
    values.sort();
    values.dedup();
    values
}

fn compact_source(source: &str) -> String {
    source
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect()
}

fn occurrences(source: &str, needle: &str) -> usize {
    source.match_indices(needle).count()
}

fn runtime_service_sources_are_manifest_bound(sources: &ProductionSources<'_>) -> bool {
    let control = compact_source(sources.control_service_source);
    let data = compact_source(sources.data_service_source);
    occurrences(&control, ".add_service(") == 4
        && occurrences(&control, ".add_service(services.agent_control(control))") == 2
        && occurrences(&control, ".add_service(services.replica_peer(peer))") == 2
        && occurrences(
            &control,
            "proto::agent_control_server::AgentControlServer::new(service)",
        ) == 1
        && occurrences(
            &control,
            "proto::replica_peer_server::ReplicaPeerServer::new(service)",
        ) == 1
        && occurrences(&data, ".add_service(") == 2
        && occurrences(
            &data,
            ".add_service(PUBLIC_V1_DATA_SERVICES.carrier(service))",
        ) == 2
        && occurrences(
            &data,
            "#[cfg(test)]pub(super)fntest_listen(self:&Arc<Self>,listener:TcpListener)",
        ) == 1
        && occurrences(&data, "wire::carrier_server::CarrierServer::new(service)") == 1
}

fn factory_source_is_manifest_bound(sources: &ProductionSources<'_>) -> bool {
    let factory = compact_source(sources.factory_source);
    let native = compact_source(sources.native_module_source);
    let adapter = compact_source(sources.service_adapter_source);
    let production = compact_source(sources.native_production_source);
    occurrences(&factory, "PUBLIC_V1_FACTORIES.construct_default()") == 2
        && occurrences(
            &factory,
            "pubfnnew()->Self{PUBLIC_V1_FACTORIES.construct_default()}",
        ) == 1
        && occurrences(
            &factory,
            "ReplicatorFactorySelection::Builtin=>{PUBLIC_V1_FACTORIES.construct_default().create_replicator(context,state_provider.clone(),settings.clone()).await}",
        ) == 1
        && occurrences(
            &factory,
            "v1::create_production(context,state_provider,settings).await",
        ) == 1
        && !factory.contains("DurableState")
        && !factory.contains("StatelessReplicatorFactory")
        && !factory.contains("pubmodstateless;")
        && occurrences(&factory, "Factory::") == 0
        && occurrences(&native, "pub(crate)useservice_adapter::create_production;") == 1
        && occurrences(
            &adapter,
            "pub(crate)useproduction::createascreate_production;",
        ) == 1
        && occurrences(&adapter, "modproduction;") == 1
        && occurrences(&adapter, "modproduction_transport;") == 1
        && [
            "context.claim_native_creation()?",
            "Transport::start(",
            "ServiceReplicator::new(",
            "FacadeSetup::new(adapter,config).start(",
            "Arc::new(Production{",
            "implReplicatorforProduction{",
            "implPrimaryReplicatorforProduction{",
            "implStateReplicatorforProduction{",
            "Ok(ReplicatorInterfaces::primary(",
            ".with_effective_replication_address(local.address)",
        ]
        .into_iter()
        .all(|binding| occurrences(&production, binding) == 1)
        && !production.contains("coordinator::")
        && !production.contains("with_native_runtime_control")
}

fn controller_source_is_manifest_bound(sources: &ProductionSources<'_>) -> bool {
    let controller = compact_source(sources.controller_startup_source);
    occurrences(
        &controller,
        "letcontroller_manifest=public_v1_registration::PUBLIC_V1_CONTROLLER;",
    ) == 1
        && occurrences(&controller, "controller_manifest.bind_reconciler(") == 1
        && occurrences(&controller, "controller_manifest.bind_owners(") == 1
        && occurrences(&controller, "Controller::new(") == 1
        && occurrences(&controller, ".owns(") == 4
        && occurrences(&controller, ".owns(owners.pods,watcher::Config::default())") == 1
        && occurrences(
            &controller,
            ".owns(owners.persistent_volume_claims,watcher::Config::default())",
        ) == 1
        && occurrences(
            &controller,
            ".owns(owners.services,watcher::Config::default())",
        ) == 1
        && occurrences(
            &controller,
            ".owns(owners.secrets,watcher::Config::default())",
        ) == 1
}

fn discover_inventory(sources: &ProductionSources<'_>) -> ReplacementOwnershipInventory {
    let mut factories = sorted_unique(
        sources
            .factory_registrations
            .iter()
            .copied()
            .map(|registration| registration.name().to_string())
            .collect(),
    );
    if !factory_source_is_manifest_bound(sources) {
        factories.push("unmanifested built-in factory construction".to_string());
    }
    let mut services = sources
        .control_service_manifest
        .registrations()
        .into_iter()
        .map(|registration| registration.name().to_string())
        .collect::<Vec<_>>();
    services.extend(
        sources
            .data_service_manifest
            .registrations()
            .into_iter()
            .map(|registration| registration.name().to_string()),
    );
    let mut services = sorted_unique(services);
    if !runtime_service_sources_are_manifest_bound(sources) {
        services.push("unmanifested runtime service registration".to_string());
    }
    let store_tables = sorted_unique(
        sources
            .store_tables
            .iter()
            .map(|name| (*name).to_string())
            .collect(),
    );
    let startup_owners = sources
        .service_startup_owners
        .iter()
        .chain(sources.process_startup_owners.iter())
        .map(|name| (*name).to_string())
        .collect();
    let protocol_versions = sources
        .protocol_versions
        .iter()
        .map(|name| (*name).to_string())
        .collect();
    let mut controller_modules = sources
        .controller_manifest
        .registrations()
        .into_iter()
        .map(|registration| registration.name().to_string())
        .collect::<Vec<_>>();
    if !controller_source_is_manifest_bound(sources) {
        controller_modules.push("unmanifested controller startup helper".to_string());
    }
    ReplacementOwnershipInventory {
        factories,
        services,
        host_attachments: sorted_unique(
            sources
                .host_attachments
                .iter()
                .map(|name| (*name).to_string())
                .collect(),
        ),
        report_branches: sorted_unique(
            sources
                .report_branches
                .iter()
                .map(|name| (*name).to_string())
                .collect(),
        ),
        store_tables,
        startup_owners: sorted_unique(startup_owners),
        protocol_versions: sorted_unique(protocol_versions),
        controller_modules: sorted_unique(controller_modules),
    }
}

pub fn replacement_ownership_inventory() -> ReplacementOwnershipInventory {
    discover_inventory(&production_sources())
}

pub fn replacement_ownership_inventory_with_extra_endpoint_fixture() -> ReplacementOwnershipInventory
{
    discover_inventory(&ProductionSources {
        control_service_source: CONTROL_SERVICE_WITH_UNMANIFESTED_ENDPOINT,
        ..production_sources()
    })
}

pub fn replacement_ownership_inventory_with_alternate_factory_fixture()
-> ReplacementOwnershipInventory {
    let mut sources = production_sources();
    sources.factory_registrations.push(
        super::public_v1_alternate_registration_fixture::alternate_builtin_factory_registration(),
    );
    discover_inventory(&sources)
}

pub fn replacement_ownership_inventory_with_alternate_protocol_fixture()
-> ReplacementOwnershipInventory {
    let protocol_versions = ["PUBLIC_V1_PROTOCOL_VERSION", "ALTERNATE_PROTOCOL_VERSION"];
    discover_inventory(&ProductionSources {
        protocol_versions: &protocol_versions,
        ..production_sources()
    })
}

pub fn replacement_ownership_inventory_with_alternate_controller_fixture()
-> ReplacementOwnershipInventory {
    discover_inventory(&ProductionSources {
        controller_startup_source: CONTROLLER_SOURCE_WITH_UNMANIFESTED_HELPER,
        ..production_sources()
    })
}

pub fn validate_replacement_ownership_inventory(
    inventory: &ReplacementOwnershipInventory,
) -> Result<(), String> {
    let reject = |category: &str, observed: &[String]| {
        Err(format!(
            "replacement {category} registrations changed: {observed:?}"
        ))
    };
    if inventory.factories.len() != 1
        || !inventory
            .factories
            .iter()
            .any(|name| name == "DefaultReplicatorFactory")
    {
        return reject("factory", &inventory.factories);
    }
    if inventory.services.len() != 3
        || !inventory.services.iter().all(|name| {
            matches!(
                name.as_str(),
                "AgentControlServer" | "ReplicaPeerServer" | "CarrierServer"
            )
        })
    {
        return reject("service", &inventory.services);
    }
    if inventory.host_attachments.len() != 3
        || !inventory.host_attachments.iter().all(|name| {
            matches!(
                name.as_str(),
                "PrimaryReplicator" | "Replicator" | "StateProvider"
            )
        })
    {
        return reject("attachment", &inventory.host_attachments);
    }
    if inventory.report_branches.as_slice() != ["public_v1"] {
        return reject("report", &inventory.report_branches);
    }
    if inventory.store_tables.as_slice() != ["agent_state"] {
        return reject("table", &inventory.store_tables);
    }
    if inventory.startup_owners.len() != 2
        || !inventory
            .startup_owners
            .iter()
            .all(|name| matches!(name.as_str(), "PartitionOperationRuntime" | "serve_control"))
    {
        return reject("startup owner", &inventory.startup_owners);
    }
    if inventory.protocol_versions.as_slice() != ["PUBLIC_V1_PROTOCOL_VERSION"] {
        return reject("protocol version", &inventory.protocol_versions);
    }
    if inventory.controller_modules.as_slice()
        != [
            "owner:PersistentVolumeClaim",
            "owner:Pod",
            "owner:Secret",
            "owner:Service",
            "reconciler",
        ]
    {
        return reject("controller module", &inventory.controller_modules);
    }
    if REPLICATOR.contains("LegacyManaged")
        || REPLICATOR.contains("pub struct DefaultReplicator {")
        || PUBLIC_LIFECYCLE.contains("ManagedReplicatorCapabilities")
        || HOST_PROCESS.contains("run_outbound(")
        || HOST_PROCESS.contains("run_peer_discovery(")
        || sources_contain_alternate_protocol(&production_sources())
    {
        return Err("legacy replacement owner remains in production source".into());
    }
    Ok(())
}

fn sources_contain_alternate_protocol(sources: &ProductionSources<'_>) -> bool {
    [
        sources.public_operations,
        sources.host_operation,
        sources.host_lifecycle_program,
    ]
    .into_iter()
    .any(|source| {
        source.contains("is_legacy_preview")
            || source.contains("PUBLIC_OPERATION_PREVIEW_PROTOCOL_VERSION")
            || source.contains("protocol-10")
            || source.contains("PreviewServiceApi")
            || source.contains("execute_preview_service_location")
            || source.contains("HostedPublicV1Adapter")
    })
}

fn replacement_ownership_gate() {
    crate::replicator::validate_public_v1_factories(crate::replicator::PUBLIC_V1_FACTORIES)
        .unwrap();
    crate::host::service::validate_public_v1_control_services(
        crate::host::service::PUBLIC_V1_CONTROL_SERVICES,
    )
    .unwrap();
    assert_eq!(
        crate::replicator::v1::PUBLIC_V1_DATA_SERVICES
            .registrations()
            .map(|registration| registration.name()),
        ["CarrierServer"],
    );
    controller_registration::validate_public_v1_controller_registrations(
        controller_registration::PUBLIC_V1_CONTROLLER,
    )
    .unwrap();
    let inventory = replacement_ownership_inventory();
    validate_replacement_ownership_inventory(&inventory).unwrap();
    let alternate_native_owner = SERVICE_ADAPTER_SOURCE.replacen(
        "production::create as create_production",
        "alternate::create as create_production",
        1,
    );
    let alternate_native_factory = discover_inventory(&ProductionSources {
        service_adapter_source: &alternate_native_owner,
        ..production_sources()
    });
    let alternate_composition = NATIVE_PRODUCTION_SOURCE.replacen(
        ".with_effective_replication_address(local.address)",
        ".with_alternate_replication_address(local.address)",
        1,
    );
    assert_ne!(
        alternate_composition, NATIVE_PRODUCTION_SOURCE,
        "composition mutation must alter the actual ordinary endpoint registration"
    );
    let alternate_native_composition = discover_inventory(&ProductionSources {
        native_production_source: &alternate_composition,
        ..production_sources()
    });
    let concrete_sdk = compact_source(REPLICATOR);
    let sdk_dispatch =
        "ReplicatorFactorySelection::Builtin=>{PUBLIC_V1_FACTORIES.construct_default()";
    let alternate_sdk = concrete_sdk.replacen(
        sdk_dispatch,
        "ReplicatorFactorySelection::Builtin=>{unmanifested_builtin()",
        1,
    );
    assert_ne!(
        alternate_sdk, concrete_sdk,
        "SDK mutation must alter the actual concrete builtin constructor"
    );
    let alternate_sdk_factory = discover_inventory(&ProductionSources {
        factory_source: &alternate_sdk,
        ..production_sources()
    });
    for (category, mutation) in [
        ("unmanifested concrete SDK factory", alternate_sdk_factory),
        (
            "unmanifested native factory owner",
            alternate_native_factory,
        ),
        (
            "unmanifested native interface composition",
            alternate_native_composition,
        ),
        (
            "unmanifested runtime service",
            replacement_ownership_inventory_with_extra_endpoint_fixture(),
        ),
        (
            "unmanifested built-in factory",
            replacement_ownership_inventory_with_alternate_factory_fixture(),
        ),
        (
            "alternate protocol",
            replacement_ownership_inventory_with_alternate_protocol_fixture(),
        ),
        (
            "unmanifested controller startup helper",
            replacement_ownership_inventory_with_alternate_controller_fixture(),
        ),
    ] {
        assert!(
            validate_replacement_ownership_inventory(&mutation).is_err(),
            "canonical replacement eligibility accepted {category}"
        );
    }
}

const GATES: [Gate; 16] = [
    Gate {
        id: "KSF-01",
        name: "primary_configuration_follows_primary_role",
        owner: "lifecycle",
        postgres_scenarios: &["public_v1_postgres_primary_lifecycle_and_joint_configuration"],
        replacement_scenarios: &[
            "replicator::v1::role::tests::initial_primary_queries_provider_before_epoch_and_requires_configuration",
            "host::service::ordinary_goal_tests::ordinary_goal_build_is_retained_exact_two_ended_and_remove_is_local_only",
            "retained_secondary_promotion_preserves_acknowledged_bytes_and_replaces_service_location",
            "m2_builtin_multi_secondary_joint_current_preserves_exact_sessions",
        ],
        expected: "primary role completes before primary configuration",
        eligibility: contract_gate,
    },
    Gate {
        id: "KSF-02",
        name: "authorized_data_loss_false_true_error_ambiguity",
        owner: "recovery",
        postgres_scenarios: &[],
        replacement_scenarios: &[
            "helper_free_recovery_scopes_changed_unchanged_and_failed_observations",
        ],
        expected: "logical authorization distinguishes every callback outcome",
        eligibility: contract_gate,
    },
    Gate {
        id: "KSF-03",
        name: "same_role_secondary_receives_new_epoch",
        owner: "lifecycle",
        postgres_scenarios: &["public_v1_postgres_primary_lifecycle_and_joint_configuration"],
        replacement_scenarios: &[
            "m2_builtin_crossed_identity_and_stale_ack",
            "public_v1_peer_withdrawal_advances_epoch_without_role_replay",
        ],
        expected: "same-role epoch renewal fences predecessor traffic",
        eligibility: contract_gate,
    },
    Gate {
        id: "KSF-04",
        name: "configuration_contains_only_remote_active_members",
        owner: "configuration",
        postgres_scenarios: &["public_v1_postgres_primary_lifecycle_and_joint_configuration"],
        replacement_scenarios: &[
            "m2_builtin_target_context_and_ack_boundaries",
            "m2_builtin_multi_secondary_joint_current_preserves_exact_sessions",
            "replicator::v1::service_adapter::tests::facade::exact_binding_and_configuration_failures_do_not_start_providers_or_mutate_owners",
        ],
        expected: "configuration contains only exact ready remote peers",
        eligibility: contract_gate,
    },
    Gate {
        id: "KSF-05",
        name: "build_uses_invalid_target_progress_and_exact_attempt",
        owner: "build",
        postgres_scenarios: &["public_v1_postgres_build_cancellation_and_retirement"],
        replacement_scenarios: &[
            "host::service::ordinary_goal_tests::ordinary_goal_build_is_retained_exact_two_ended_and_remove_is_local_only",
            "m2_builtin_target_context_and_ack_boundaries",
            "m2_builtin_lost_target_cancel_quarantines_receiver",
            "m2_builtin_crossed_identity_and_stale_ack",
            "public_build_consumes_target_context_and_installs_nonempty_snapshot",
        ],
        expected: "build target is idle, outside PC/CC, and has invalid progress",
        eligibility: contract_gate,
    },
    Gate {
        id: "KSF-06",
        name: "idle_removal_cancels_session_and_retains_callback_drain_ownership",
        owner: "build",
        postgres_scenarios: &["public_v1_postgres_build_cancellation_and_retirement"],
        replacement_scenarios: &[
            "m2_builtin_effect_fences_and_retained_provider_join",
            "m2_builtin_abort_close_retains_all_joins",
            "m2_application_sqlite_abort_close_retains_actual_sql_worker",
            "m2_application_scope_withdrawal_waits_for_actual_kv_consumer_close",
            "m2_builtin_exact_session_replay_and_retirement",
        ],
        expected: "Remove cancels the exact idle session without late install; retained callbacks drain under Close/Abort ownership",
        eligibility: contract_gate,
    },
    Gate {
        id: "KSF-07",
        name: "swap_uses_captured_specific_quorum_capability",
        owner: "swap",
        postgres_scenarios: &[],
        replacement_scenarios: &[
            "native_swap_freezes_specific_quorum_and_conservative_all_fallback_inputs",
            "host::public_lifecycle::tests::sf06_swap_retains_primary_through_catchup_then_pairs_actual_roles",
            "replicator::v1::replica_manager::tests::multiple_must_catchup_replicas_must_reach_advancing_current_quorum_not_saved_b",
            "replicator::v1::replica_manager::tests::all_wait_tracks_live_writes_and_completed_wait_does_not_reopen",
        ],
        expected: "specific quorum and fallback are captured replay inputs",
        eligibility: contract_gate,
    },
    Gate {
        id: "KSF-08",
        name: "progress_is_role_correct_and_history_scoped",
        owner: "progress",
        postgres_scenarios: &["public_v1_postgres_data_loss_history_and_replay"],
        replacement_scenarios: &[
            "public_v1_history_partition_seed_is_shared",
            "native_query_outcome_matrix_survives_rpc_persistence_and_controller_eligibility",
            "native_query_not_queried_preserves_copy_detail_and_cannot_elect_or_build",
            "native_query_rejoin_matrix_distinguishes_source_capability_from_target_current",
            "public_v1_evidence_report_is_structurally_public_only",
            "helper_free_recovery_scopes_changed_unchanged_and_failed_observations",
            "sf01_primary_local_commit_hides_speculation_after_native_completion",
            "sf01_dropped_client_retains_service_owned_commit_and_observation",
            "sf11_acknowledged_prefix_6_through_10_survives_ambiguous_11_without_history_fabrication",
            "m2_builtin_crossed_identity_and_stale_ack",
        ],
        expected: "numeric progress is comparable only inside one history",
        eligibility: contract_gate,
    },
    Gate {
        id: "KSF-09",
        name: "replacement_bundle_has_no_managed_capability",
        owner: "construction",
        postgres_scenarios: &[],
        replacement_scenarios: &[
            "production_config_has_no_protocol_selector_or_application_commit_oracle",
        ],
        expected: "private native Public V1 is the sole built-in default",
        eligibility: replacement_ownership_gate,
    },
    Gate {
        id: "KSF-10",
        name: "replicator_open_owns_endpoint_lifetime",
        owner: "transport",
        postgres_scenarios: &[],
        replacement_scenarios: &[
            "host::service::ordinary_goal_tests::ordinary_goal_builtin_factory_opens_actual_engine_listener_and_writer",
            "m2_builtin_abort_close_retains_all_joins",
            "m2_application_sqlite_close_joins_actual_consumers_and_retains_failure",
        ],
        expected: "public endpoint lifetime follows Replicator lifecycle",
        eligibility: replacement_ownership_gate,
    },
    Gate {
        id: "KSF-11",
        name: "role_address_is_published_and_cleared",
        owner: "reporting",
        postgres_scenarios: &[],
        replacement_scenarios: &[
            "retained_secondary_promotion_preserves_acknowledged_bytes_and_replaces_service_location",
            "public_factory_primary_process_renewal_retains_exact_role_address_authority",
        ],
        expected: "exact role completion publishes and clears address",
        eligibility: contract_gate,
    },
    Gate {
        id: "KSF-12",
        name: "supersession_cancels_and_drains_public_callbacks",
        owner: "operation",
        postgres_scenarios: &["public_v1_postgres_build_cancellation_and_retirement"],
        replacement_scenarios: &[
            "m2_application_sqlite_abort_close_retains_actual_sql_worker",
            "m2_builtin_lost_target_cancel_quarantines_receiver",
            "m2_builtin_abort_close_retains_all_joins",
        ],
        expected: "supersession contains callback roots and descendants",
        eligibility: contract_gate,
    },
    Gate {
        id: "KSF-13",
        name: "transient_fault_restarts_or_drops_exact_incarnation",
        owner: "fault",
        postgres_scenarios: &[],
        replacement_scenarios: &[
            "native_transient_fault_restarts_same_storage_under_exact_session_renewal",
            "native_controller_unreachable_survivor_fences_until_exact_return_or_explicit_retirement",
            "primary_commit_failure_contains_the_ambiguous_native_quorum_success",
            "m2_builtin_lost_target_cancel_quarantines_receiver",
            "m2_application_copy_failures_preserve_retained_bytes_and_never_ack",
            "m2_application_kv_ordinary_copy_access_and_matching_format_reopen",
        ],
        expected: "fault closes access and targets one exact incarnation",
        eligibility: contract_gate,
    },
    Gate {
        id: "KSF-14",
        name: "contained_close_preserves_child_diagnostic",
        owner: "close",
        postgres_scenarios: &["public_v1_postgres_contained_close"],
        replacement_scenarios: &[
            "host::service::ordinary_goal_tests::ordinary_goal_builtin_factory_opens_actual_engine_listener_and_writer",
            "m2_builtin_abort_close_retains_all_joins",
            "host::service::ordinary_goal_tests::ordinary_goal_terminal_ids_reject_before_mutation_and_exact_retries_reopen",
        ],
        expected: "contained child Close failure preserves diagnostics after successful outer cleanup",
        eligibility: contract_gate,
    },
    Gate {
        id: "KSF-15",
        name: "callback_replay_follows_declared_disposition",
        owner: "replay",
        postgres_scenarios: &[],
        replacement_scenarios: &[
            "in_process::journeys::sf01_primary_local_commit_isolation_and_abort_never_late_grants_success",
            "in_process::journeys::failover_recreation_at_persisted_promotion_preserves_effects",
            "in_process::journeys::failover_recreation_after_durable_promotion_consumes_completion_once",
            "in_process::journeys::switchover_recreation_after_catch_up_resumes_accepted_handoff",
            "in_process::journeys::switchover_recreation_after_durable_handoff_resumes_promotion",
            "host::service::ordinary_goal_tests::ordinary_goal_builtin_factory_opens_actual_engine_listener_and_writer",
            "m2_builtin_exact_session_replay_and_retirement",
            "host::service::ordinary_goal_tests::ordinary_goal_terminal_ids_reject_before_mutation_and_exact_retries_reopen",
            "public_v1_replacement_schema_failed_build_containment_survives_reopen",
            "host::service::ordinary_goal_tests::m2_ordinary_rebuild_error_preserves_exact_goal_wire_and_reopen",
            "public_v1_history_data_loss_attempt_renews_higher_epoch",
            "public_v1_data_loss_renewal_requires_exact_replay_capability",
            "public_v1_replacement_schema_build_ambiguity_requires_retirement",
        ],
        expected: "journal replay preserves ambiguity and capability policy",
        eligibility: contract_gate,
    },
    Gate {
        id: "KSF-16",
        name: "cutover_artifact_contains_no_legacy_continuation_owner",
        owner: "cutover",
        postgres_scenarios: &[],
        replacement_scenarios: &[
            "m2_builtin_multi_secondary_joint_current_preserves_exact_sessions",
            "m2_builtin_crossed_identity_and_stale_ack",
            "m2_application_copy_failures_preserve_retained_bytes_and_never_ack",
            "m2_builtin_bound_ingress_rejects_body_epochs_before_native_mutation",
            "replicator::v1::transport::tests::malformed_presence_version_batch_and_full_route_are_rejected_not_delivered",
            "m2_builtin_exact_session_replay_and_retirement",
        ],
        expected: "current artifact inventories and rejects legacy continuation owners",
        eligibility: replacement_ownership_gate,
    },
];

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("runtime crate is inside the workspace")
        .to_path_buf()
}

const NATIVE_PARTITION: &str = "native-controller";
const NATIVE_PACKAGES: &[&str] = &[
    "kvstore2",
    "kuberic-runtime",
    "kuberic-controller",
    "kuberic-public-v1-tests",
];
const SERVICE_PACKAGES: &[&str] = &["kuberic-level-tests"];

// Real service recreation journeys partition validation work, not native behavior.
// Each retains the same shared inventory/execution budget instead of extending it.
const SERVICE_PARTITIONS: &[(&str, &str)] = &[
    (
        "failover-after-durable-promotion",
        "in_process::journeys::failover_recreation_after_durable_promotion_consumes_completion_once",
    ),
    (
        "failover-at-persisted-promotion",
        "in_process::journeys::failover_recreation_at_persisted_promotion_preserves_effects",
    ),
    (
        "ambiguous-commit-abort",
        "in_process::journeys::sf01_primary_local_commit_isolation_and_abort_never_late_grants_success",
    ),
    (
        "switchover-after-catch-up",
        "in_process::journeys::switchover_recreation_after_catch_up_resumes_accepted_handoff",
    ),
    (
        "switchover-after-durable-handoff",
        "in_process::journeys::switchover_recreation_after_durable_handoff_resumes_promotion",
    ),
];

#[derive(Clone, Debug)]
struct ExecutionPartition {
    id: &'static str,
    packages: &'static [&'static str],
    scenarios: Vec<&'static str>,
}

fn registered_replacement_scenarios() -> BTreeSet<&'static str> {
    GATES
        .iter()
        .flat_map(|gate| gate.replacement_scenarios.iter().copied())
        .collect()
}

fn execution_partitions() -> Vec<ExecutionPartition> {
    let native = ExecutionPartition {
        id: NATIVE_PARTITION,
        packages: NATIVE_PACKAGES,
        scenarios: registered_replacement_scenarios()
            .into_iter()
            .filter(|scenario| !scenario.starts_with("in_process::journeys::"))
            .collect(),
    };
    std::iter::once(native)
        .chain(
            SERVICE_PARTITIONS
                .iter()
                .map(|&(id, scenario)| ExecutionPartition {
                    id,
                    packages: SERVICE_PACKAGES,
                    scenarios: vec![scenario],
                }),
        )
        .collect()
}

fn validate_execution_partitions(partitions: &[ExecutionPartition]) -> Result<(), String> {
    let registered = registered_replacement_scenarios();
    let mut ids = BTreeSet::new();
    let mut observed = BTreeSet::new();
    for partition in partitions {
        if !ids.insert(partition.id) || partition.scenarios.is_empty() {
            return Err(format!(
                "duplicate or empty execution partition: {}",
                partition.id
            ));
        }
        if partition.id == NATIVE_PARTITION {
            if partition.packages != NATIVE_PACKAGES
                || partition
                    .scenarios
                    .iter()
                    .any(|name| name.starts_with("in_process::journeys::"))
            {
                return Err("native partition crossed its package/scenario boundary".into());
            }
        } else {
            let Some(&(_, scenario)) = SERVICE_PARTITIONS
                .iter()
                .find(|&&(id, _)| id == partition.id)
            else {
                return Err(format!(
                    "unregistered execution partition: {}",
                    partition.id
                ));
            };
            if partition.packages != SERVICE_PACKAGES || partition.scenarios != [scenario] {
                return Err(format!(
                    "service partition must select its one exact journey: {}",
                    partition.id
                ));
            }
        }
        for &scenario in &partition.scenarios {
            if !registered.contains(scenario) || !observed.insert(scenario) {
                return Err(format!(
                    "unregistered or overlapping partition scenario: {scenario}"
                ));
            }
        }
    }
    let expected_ids = std::iter::once(NATIVE_PARTITION)
        .chain(SERVICE_PARTITIONS.iter().map(|&(id, _)| id))
        .collect::<BTreeSet<_>>();
    if ids != expected_ids || observed != registered {
        return Err(format!(
            "partition coverage mismatch: missing partitions {:?}, missing scenarios {:?}, extra scenarios {:?}",
            expected_ids.difference(&ids).collect::<Vec<_>>(),
            registered.difference(&observed).collect::<Vec<_>>(),
            observed.difference(&registered).collect::<Vec<_>>(),
        ));
    }
    Ok(())
}

pub fn assert_public_v1_conformance_partition_coverage() {
    let partitions = execution_partitions();
    validate_execution_partitions(&partitions).unwrap();

    let mut omitted = partitions.clone();
    omitted[0].scenarios.pop();
    let mut overlapping = partitions.clone();
    overlapping[0].scenarios.push(partitions[0].scenarios[0]);
    let mut missing_partition = partitions.clone();
    missing_partition.pop();
    let mut unregistered = partitions.clone();
    unregistered[0].scenarios.push("unregistered-observation");
    let mut broadened = partitions.clone();
    broadened[1].packages = NATIVE_PACKAGES;
    for mutation in [
        omitted,
        overlapping,
        missing_partition,
        unregistered,
        broadened,
    ] {
        assert!(
            validate_execution_partitions(&mutation).is_err(),
            "execution partition validator accepted missing, overlapping, or broadened coverage",
        );
    }

    assert_exact_inventory_fixtures();
}

fn scenario_filter(scenarios: &[&str]) -> String {
    scenarios
        .iter()
        .map(|scenario| format!("test(={scenario})"))
        .collect::<Vec<_>>()
        .join(" or ")
}

fn validate_scenario_inventory(
    inventory: &serde_json::Value,
    partition: &ExecutionPartition,
) -> Result<(), String> {
    let expected = partition
        .scenarios
        .iter()
        .map(|name| (*name).to_owned())
        .collect::<BTreeSet<_>>();
    let suites = inventory
        .get("rust-suites")
        .and_then(serde_json::Value::as_object)
        .ok_or("conformance inventory has no Rust suites")?;
    let mut selected = BTreeSet::new();
    for (binary, suite) in suites {
        let package = suite
            .get("package-name")
            .and_then(serde_json::Value::as_str)
            .ok_or("conformance inventory has no package name")?;
        let cases = suite
            .get("testcases")
            .and_then(serde_json::Value::as_object)
            .ok_or("conformance inventory has no test cases")?;
        for (name, case) in cases {
            if case["filter-match"]["status"] != "matches" {
                continue;
            }
            if case.get("ignored").and_then(serde_json::Value::as_bool) != Some(false) {
                return Err(format!(
                    "partition {} selected an ignored case: {binary}::{name}",
                    partition.id
                ));
            }
            if !partition.packages.contains(&package) || !expected.contains(name) {
                return Err(format!(
                    "partition {} selected an unexpected case: {binary}::{name}",
                    partition.id
                ));
            }
            if !selected.insert(name.clone()) {
                return Err(format!(
                    "partition {} selected the same observation in multiple binaries: {name}",
                    partition.id
                ));
            }
        }
    }
    if selected != expected {
        return Err(format!(
            "partition {} omitted exact scenarios: {:?}",
            partition.id,
            expected.difference(&selected).collect::<Vec<_>>(),
        ));
    }
    Ok(())
}

fn assert_exact_inventory_fixtures() {
    let partition = ExecutionPartition {
        id: "exact-inventory-fixture",
        packages: NATIVE_PACKAGES,
        scenarios: vec!["exact-observation"],
    };
    let inventory = serde_json::json!({
        "rust-suites": {
            "kuberic-runtime::fixture": {
                "package-name": "kuberic-runtime",
                "testcases": {
                    "exact-observation": {
                        "ignored": false,
                        "filter-match": {"status": "matches"},
                    },
                },
            },
        },
    });
    validate_scenario_inventory(&inventory, &partition).unwrap();
    let mut omitted = inventory.clone();
    omitted["rust-suites"]["kuberic-runtime::fixture"]["testcases"]
        .as_object_mut()
        .unwrap()
        .clear();
    let mut ignored = inventory.clone();
    ignored["rust-suites"]["kuberic-runtime::fixture"]["testcases"]["exact-observation"]["ignored"] =
        true.into();
    let mut suffix = inventory.clone();
    let cases = suffix["rust-suites"]["kuberic-runtime::fixture"]["testcases"]
        .as_object_mut()
        .unwrap();
    let case = cases.remove("exact-observation").unwrap();
    cases.insert("exact-observation-suffix".into(), case);
    let mut foreign = inventory.clone();
    foreign["rust-suites"]["kuberic-runtime::fixture"]["package-name"] =
        "kuberic-level-tests".into();
    let mut duplicated = inventory.clone();
    duplicated["rust-suites"]["kuberic-runtime::duplicate"] =
        inventory["rust-suites"]["kuberic-runtime::fixture"].clone();
    for mutation in [omitted, ignored, suffix, foreign, duplicated] {
        assert!(
            validate_scenario_inventory(&mutation, &partition).is_err(),
            "exact inventory validator accepted missing, ignored, prefix-only, crossed, or duplicate evidence",
        );
    }
}

fn conformance_command(
    action: &str,
    root: &Path,
    archive: Option<&Path>,
    partition: &ExecutionPartition,
) -> Command {
    let mut command = Command::new("cargo");
    command.current_dir(root).args(["nextest", action]);
    if let Some(archive) = archive {
        command.arg("--archive-file").arg(archive);
    } else {
        // Match the source job's feature graph instead of rebuilding a package subset.
        command.args(["--workspace", "--all-features"]);
    }
    if action == "run" {
        let threads = if partition.id == NATIVE_PARTITION {
            "2"
        } else {
            "1"
        };
        command.args(["-j", threads]);
    }
    command
}

fn run_scenarios(subject: &str, partition: &ExecutionPartition, deadline: CommandDeadline) {
    let root = workspace_root();
    let packages = partition
        .packages
        .iter()
        .map(|package| format!("package(={package})"))
        .collect::<Vec<_>>()
        .join(" or ");
    let filter = format!(
        "({packages}) and ({})",
        scenario_filter(&partition.scenarios)
    );
    let archive = std::env::var_os("NEXTEST_ARCHIVE")
        .map(PathBuf::from)
        .map(|path| {
            if path.is_absolute() {
                path
            } else {
                root.join(path)
            }
        })
        .filter(|path| path.is_file());
    let mut list = conformance_command("list", &root, archive.as_deref(), partition);
    list.args([
        "--message-format",
        "json",
        "--profile",
        "ordinary",
        "--ignore-default-filter",
        "-E",
        &filter,
    ]);
    let inventory_started = Instant::now();
    eprintln!(
        "Public V1 partition {}: inventory and bounded execution share {:?}",
        partition.id,
        deadline.work.saturating_duration_since(inventory_started),
    );
    let list = bounded_output(&mut list, deadline, "list Public V1 conformance scenarios")
        .unwrap_or_else(|error| panic!("{error}"));
    assert!(
        list.status.success(),
        "replacement Public V1 conformance inventory failed: {}",
        String::from_utf8_lossy(&list.stderr)
    );
    let inventory = serde_json::from_slice(&list.stdout).expect("nextest inventory is JSON");
    validate_scenario_inventory(&inventory, partition).unwrap();
    eprintln!(
        "Public V1 partition {}: exact inventory validated in {:?}; remaining shared budget {:?}",
        partition.id,
        inventory_started.elapsed(),
        deadline.work.saturating_duration_since(Instant::now()),
    );

    let mut run = conformance_command("run", &root, archive.as_deref(), partition);
    let extract = archive.as_ref().map(|_| {
        let path = root.join("target").join(format!(
            "public-v1-conformance-extract-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("create Public V1 conformance extraction directory");
        run.arg("--extract-to")
            .arg(&path)
            .arg("--workspace-remap")
            .arg(&root);
        path
    });
    run.args([
        "--profile",
        "ordinary",
        "--ignore-default-filter",
        "-E",
        &filter,
    ]);
    let execution_started = Instant::now();
    let result = bounded_output(
        &mut run,
        deadline,
        &format!(
            "run {subject} Public V1 conformance partition {}",
            partition.id
        ),
    );
    if let Some(extract) = extract {
        let _ = std::fs::remove_dir_all(extract);
    }
    let run = result.unwrap_or_else(|error| panic!("{error}"));
    assert!(
        run.status.success(),
        "{subject} Public V1 conformance scenarios failed:\n{}\n{}",
        String::from_utf8_lossy(&run.stdout),
        String::from_utf8_lossy(&run.stderr)
    );
    eprintln!(
        "Public V1 partition {}: bounded execution passed in {:?}\n{}\n{}",
        partition.id,
        execution_started.elapsed(),
        String::from_utf8_lossy(&run.stdout),
        String::from_utf8_lossy(&run.stderr),
    );
}

#[cfg(all(test, unix))]
mod bounded_process_tests {
    use super::*;

    #[test]
    fn nested_commands_reuse_workspace_features_or_the_exact_archive() {
        for partition in execution_partitions() {
            for action in ["list", "run"] {
                for archive in [None, Some(Path::new("conformance.tar.zst"))] {
                    let command = conformance_command(action, Path::new("."), archive, &partition);
                    let mut expected = if archive.is_some() {
                        vec!["nextest", action, "--archive-file", "conformance.tar.zst"]
                    } else {
                        vec!["nextest", action, "--workspace", "--all-features"]
                    };
                    if action == "run" {
                        expected.extend([
                            "-j",
                            if partition.id == NATIVE_PARTITION {
                                "2"
                            } else {
                                "1"
                            },
                        ]);
                    }
                    assert_eq!(
                        command
                            .get_args()
                            .map(|argument| argument.to_str().unwrap())
                            .collect::<Vec<_>>(),
                        expected
                    );
                }
            }
        }
    }

    fn process_exists(pid: i32) -> bool {
        unsafe extern "C" {
            fn kill(pid: i32, signal: i32) -> i32;
        }
        unsafe { kill(pid, 0) == 0 }
    }

    #[test]
    fn partition_budget_reserves_cleanup_and_outer_owner_margin() {
        let started = Instant::now();
        let deadline = partition_deadline(started);
        assert_eq!(
            deadline.work.duration_since(started),
            Duration::from_secs(240)
        );
        assert_eq!(
            deadline.cleanup.duration_since(deadline.work),
            Duration::from_secs(10)
        );
        assert_eq!(
            (started + CONFORMANCE_OUTER_TIMEOUT).duration_since(deadline.cleanup),
            Duration::from_secs(20),
        );
    }

    #[test]
    fn exhausted_shared_budget_never_spawns_a_later_command() {
        let root = workspace_root()
            .join("target")
            .join(format!("conformance-exhausted-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let marker = root.join("unexpected-spawn");
        let started = Instant::now();
        let deadline = CommandDeadline::new(
            started,
            Duration::from_millis(20),
            Duration::from_millis(50),
        );
        std::thread::sleep(Duration::from_millis(30));
        let mut command = Command::new("sh");
        command.args(["-c", &format!("printf started > '{}'", marker.display())]);
        let error = bounded_output(&mut command, deadline, "exhausted subject").unwrap_err();
        assert!(error.contains("before spawn"), "{error}");
        assert!(!marker.exists());
        assert!(started.elapsed() < Duration::from_millis(200));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn direct_child_exit_with_open_descendant_pipes_is_deadline_bounded() {
        let root = workspace_root()
            .join("target")
            .join(format!("conformance-exit-pipes-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let pid_file = root.join("descendant.pid");
        let started = Instant::now();
        let deadline = CommandDeadline::new(
            started,
            Duration::from_millis(200),
            Duration::from_millis(300),
        );
        let outer_deadline = started + Duration::from_millis(800);
        let mut command = Command::new("sh");
        command.arg("-c").arg(format!(
            "trap '' TERM; sleep 30 & child=$!; printf '%s' \"$child\" > '{}'; \
             printf leader-exited; printf retained-stderr >&2; exit 0",
            pid_file.display()
        ));
        let error = bounded_output(&mut command, deadline, "exited leader subject").unwrap_err();
        assert!(error.contains("shared work deadline"), "{error}");
        assert!(error.contains("child reaped: exit status: 0"), "{error}");
        assert!(error.contains("leader-exited") && error.contains("retained-stderr"));
        assert!(Instant::now() < deadline.cleanup);
        let descendant = std::fs::read_to_string(&pid_file).unwrap().parse().unwrap();
        while process_exists(descendant) && Instant::now() < outer_deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(
            !process_exists(descendant),
            "owned descendant survived cleanup"
        );
        assert!(Instant::now() < outer_deadline);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn undrained_pipe_is_an_explicit_bounded_failure() {
        use std::os::unix::net::UnixStream;
        use std::os::unix::process::CommandExt;

        let (stdout, _held_stdout) = UnixStream::pair().unwrap();
        let (stderr, _held_stderr) = UnixStream::pair().unwrap();
        let started = Instant::now();
        let cleanup_deadline = started + Duration::from_millis(150);
        let child = Command::new("sh")
            .args(["-c", "exit 0"])
            .process_group(0)
            .spawn()
            .unwrap();
        let pid = child.id() as i32;
        let mut owner = ProcessGroupGuard {
            child,
            cleanup_deadline,
            terminated: false,
            reaped: false,
        };
        let error = cleanup_command(
            &mut owner,
            &mut CapturedPipe::new(stdout),
            &mut CapturedPipe::new(stderr),
        )
        .unwrap_err();
        assert!(error.contains("undrained stdout=true"), "{error}");
        assert!(error.contains("undrained stderr=true"), "{error}");
        assert!(owner.reaped && !process_exists(pid));
        assert!(started.elapsed() < Duration::from_millis(300));
    }

    #[test]
    fn shared_deadline_does_not_reset_and_reaps_nested_process_tree() {
        let root = workspace_root()
            .join("target")
            .join(format!("conformance-timeout-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let pid_file = root.join("descendant.pid");
        let started = Instant::now();
        let deadline = CommandDeadline::new(
            started,
            Duration::from_millis(400),
            Duration::from_millis(200),
        );
        let mut first = Command::new("sh");
        first.args(["-c", "sleep 0.25"]);
        let first = bounded_output(&mut first, deadline, "first shared-deadline subject").unwrap();
        assert!(first.status.success());

        let mut command = Command::new("sh");
        command.arg("-c").arg(format!(
            "sleep 30 & child=$!; printf '%s' \"$child\" > '{}'; wait",
            pid_file.display()
        ));
        let error =
            bounded_output(&mut command, deadline, "second shared-deadline subject").unwrap_err();
        assert!(error.contains("exceeded"));
        assert!(
            started.elapsed() < Duration::from_millis(650),
            "the second subject reset the shared deadline"
        );
        let descendant: i32 = std::fs::read_to_string(&pid_file).unwrap().parse().unwrap();
        let reaped = (0..100).any(|_| {
            if !process_exists(descendant) {
                true
            } else {
                std::thread::sleep(Duration::from_millis(10));
                false
            }
        });
        assert!(reaped, "descendant process {descendant} survived timeout");
        std::fs::remove_dir_all(root).unwrap();
    }
}

pub fn assert_public_v1_conformance_subject(subject: &str) {
    assert!(
        matches!(subject, "postgresql" | "built-in-replacement"),
        "unknown Public V1 conformance subject: {subject}"
    );
    for (index, gate) in GATES.iter().enumerate() {
        assert_eq!(gate.id, format!("KSF-{:02}", index + 1));
        assert!(!gate.name.is_empty(), "{} has no stable gate name", gate.id);
        assert!(!gate.owner.is_empty(), "{} has no component owner", gate.id);
        assert!(
            !gate.expected.is_empty(),
            "{} has no expected public result",
            gate.id
        );
        assert!(
            !gate.replacement_scenarios.is_empty(),
            "{} has no built-in replacement scenario",
            gate.id
        );
        (gate.eligibility)();
    }
}

pub fn execute_public_v1_conformance_subject(subject: &str) {
    execute_public_v1_conformance_partition(subject, NATIVE_PARTITION);
}

pub fn execute_public_v1_conformance_partition(subject: &str, partition: &str) {
    let deadline = partition_deadline(Instant::now());
    assert_public_v1_conformance_subject(subject);
    assert_eq!(
        subject, "built-in-replacement",
        "only the built-in replacement subject has an executable nested gate"
    );
    assert_public_v1_conformance_partition_coverage();
    let partition = execution_partitions()
        .into_iter()
        .find(|candidate| candidate.id == partition)
        .unwrap_or_else(|| panic!("unknown Public V1 execution partition: {partition}"));
    eprintln!(
        "Public V1 partition {}: {:?} shared work, {:?} bounded process cleanup, {:?} outer-owner margin inside {:?}",
        partition.id,
        CONFORMANCE_TIMEOUT,
        CONFORMANCE_CLEANUP_TIMEOUT,
        CONFORMANCE_OWNER_MARGIN,
        CONFORMANCE_OUTER_TIMEOUT,
    );
    run_scenarios("built-in-replacement", &partition, deadline);
}

pub fn assert_public_v1_conformance_gate(subject: &str, id: &str, scenario: &str) {
    assert!(
        matches!(subject, "postgresql" | "built-in-replacement"),
        "unknown Public V1 conformance subject: {subject}"
    );
    let gate = GATES
        .iter()
        .find(|gate| gate.id == id)
        .unwrap_or_else(|| panic!("unknown Public V1 conformance gate: {id}"));
    let scenarios = match subject {
        "postgresql" => gate.postgres_scenarios,
        "built-in-replacement" => gate.replacement_scenarios,
        _ => unreachable!("validated Public V1 conformance subject"),
    };
    assert!(
        scenarios.contains(&scenario),
        "{subject} scenario {scenario} is not registered for {id}"
    );
    (gate.eligibility)();
}

pub fn assert_public_v1_conformance_not_applicable(subject: &str, id: &str) {
    assert!(
        matches!(subject, "postgresql" | "built-in-replacement"),
        "unknown Public V1 conformance subject: {subject}"
    );
    let gate = GATES
        .iter()
        .find(|gate| gate.id == id)
        .unwrap_or_else(|| panic!("unknown Public V1 conformance gate: {id}"));
    let scenarios = match subject {
        "postgresql" => gate.postgres_scenarios,
        "built-in-replacement" => gate.replacement_scenarios,
        _ => unreachable!("validated Public V1 conformance subject"),
    };
    assert!(
        scenarios.is_empty(),
        "{subject} unexpectedly claims executable applicability for {id}"
    );
    (gate.eligibility)();
}

pub fn public_v1_postgres_conformance_scenarios() -> Vec<&'static str> {
    let mut scenarios = GATES
        .iter()
        .flat_map(|gate| gate.postgres_scenarios.iter().copied())
        .collect::<Vec<_>>();
    scenarios.sort_unstable();
    scenarios.dedup();
    scenarios
}
