use std::io::Read;
use std::path::PathBuf;
use std::process::{Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

const CONFORMANCE_TIMEOUT: Duration = Duration::from_secs(270);

#[derive(Debug)]
struct BoundedOutput {
    status: ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

fn bounded_output(
    command: &mut Command,
    deadline: Instant,
    description: &str,
) -> Result<BoundedOutput, String> {
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = command
        .spawn()
        .map_err(|error| format!("{description} failed to start: {error}"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| format!("{description} did not capture stdout"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| format!("{description} did not capture stderr"))?;
    let stdout = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let mut stdout = stdout;
        stdout.read_to_end(&mut bytes).map(|_| bytes)
    });
    let stderr = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let mut stderr = stderr;
        stderr.read_to_end(&mut bytes).map(|_| bytes)
    });

    let status = loop {
        if let Some(status) = child
            .try_wait()
            .map_err(|error| format!("{description} status failed: {error}"))?
        {
            break status;
        }
        if Instant::now() >= deadline {
            terminate_process_tree(&mut child);
            let status = child
                .wait()
                .map_err(|error| format!("{description} reap failed: {error}"))?;
            let stdout = stdout
                .join()
                .map_err(|_| format!("{description} stdout reader panicked"))?
                .map_err(|error| format!("{description} stdout read failed: {error}"))?;
            let stderr = stderr
                .join()
                .map_err(|_| format!("{description} stderr reader panicked"))?
                .map_err(|error| format!("{description} stderr read failed: {error}"))?;
            return Err(format!(
                "{description} exceeded the shared parent deadline \
                 (terminated with {status}):\nstdout:\n{}\nstderr:\n{}",
                String::from_utf8_lossy(&stdout),
                String::from_utf8_lossy(&stderr)
            ));
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let stdout = stdout
        .join()
        .map_err(|_| format!("{description} stdout reader panicked"))?
        .map_err(|error| format!("{description} stdout read failed: {error}"))?;
    let stderr = stderr
        .join()
        .map_err(|_| format!("{description} stderr reader panicked"))?
        .map_err(|error| format!("{description} stderr read failed: {error}"))?;
    Ok(BoundedOutput {
        status,
        stdout,
        stderr,
    })
}

fn terminate_process_tree(child: &mut std::process::Child) {
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
        let grace = Instant::now() + Duration::from_millis(100);
        while Instant::now() < grace {
            if child.try_wait().ok().flatten().is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
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
const HOST_PROCESS: &str = include_str!("../host/process.rs");
const PUBLIC_OPERATIONS: &str = include_str!("../protocol/public_operations.rs");
const HOST_OPERATION: &str = include_str!("../host/operation.rs");
const HOST_LIFECYCLE_PROGRAM: &str = include_str!("../host/public_lifecycle_program.rs");
const CONTROL_SERVICE_SOURCE: &str = include_str!("../host/service.rs");
const DATA_SERVICE_SOURCE: &str = include_str!("../replicator/stateless/mod.rs");
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
    data_service_manifest: crate::replicator::stateless::PublicV1DataServiceManifest,
    factory_registrations: Vec<crate::replicator::PublicV1FactoryRegistration>,
    control_service_source: &'a str,
    data_service_source: &'a str,
    factory_source: &'a str,
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
        data_service_manifest: crate::replicator::stateless::PUBLIC_V1_DATA_SERVICES,
        factory_registrations: crate::replicator::PUBLIC_V1_FACTORIES
            .registrations()
            .to_vec(),
        control_service_source: CONTROL_SERVICE_SOURCE,
        data_service_source: DATA_SERVICE_SOURCE,
        factory_source: REPLICATOR,
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
        && occurrences(&data, ".add_service(") == 1
        && occurrences(
            &data,
            ".add_service(PUBLIC_V1_DATA_SERVICES.replication_data(service))",
        ) == 1
        && occurrences(
            &data,
            "proto::replication_data_server::ReplicationDataServer::new(service)",
        ) == 1
}

fn factory_source_is_manifest_bound(sources: &ProductionSources<'_>) -> bool {
    let factory = compact_source(sources.factory_source);
    let stateless = compact_source(sources.data_service_source);
    occurrences(&factory, "PUBLIC_V1_FACTORIES.construct_default(storage)") == 1
        && occurrences(
            &factory,
            "PUBLIC_V1_FACTORIES.construct_stateless(self.storage.clone())",
        ) == 1
        && occurrences(
            &factory,
            "stateless::StatelessReplicatorFactory::from_public_v1_manifest(storage)",
        ) == 1
        && occurrences(
            &stateless,
            "super::PUBLIC_V1_FACTORIES.construct_stateless(storage)",
        ) == 1
        && occurrences(&factory, "Factory::") + occurrences(&stateless, "Factory::") == 1
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
    if inventory.factories.len() != 2
        || !inventory
            .factories
            .iter()
            .any(|name| name == "DefaultReplicatorFactory")
        || !inventory
            .factories
            .iter()
            .any(|name| name == "StatelessReplicatorFactory")
    {
        return reject("factory", &inventory.factories);
    }
    if inventory.services.len() != 3
        || !inventory.services.iter().all(|name| {
            matches!(
                name.as_str(),
                "AgentControlServer" | "ReplicaPeerServer" | "ReplicationDataServer"
            )
        })
    {
        return reject("service", &inventory.services);
    }
    if inventory.host_attachments.len() != 4
        || !inventory.host_attachments.iter().all(|name| {
            matches!(
                name.as_str(),
                "PrimaryReplicator" | "Replicator" | "StateProvider" | "StatelessRuntimeControl"
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
    crate::replicator::stateless::validate_public_v1_data_services(
        crate::replicator::stateless::PUBLIC_V1_DATA_SERVICES,
    )
    .unwrap();
    controller_registration::validate_public_v1_controller_registrations(
        controller_registration::PUBLIC_V1_CONTROLLER,
    )
    .unwrap();
    let inventory = replacement_ownership_inventory();
    validate_replacement_ownership_inventory(&inventory).unwrap();
    for (category, mutation) in [
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
            "hosted_public_v1_switchover_dispatches_exact_intent_and_accepts_matching_completion",
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
            "hosted_public_v1_authorized_data_loss_maps_only_the_effective_outcome",
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
            "hosted_public_v1_switchover_dispatches_exact_intent_and_accepts_matching_completion",
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
            "hosted_public_v1_switchover_dispatches_exact_intent_and_accepts_matching_completion",
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
            "hosted_public_v1_build_dispatches_exact_attempt_and_waits_for_matching_completion",
        ],
        expected: "build target is idle, outside PC/CC, and has invalid progress",
        eligibility: contract_gate,
    },
    Gate {
        id: "KSF-06",
        name: "build_is_cancelled_and_drained_before_remove",
        owner: "build",
        postgres_scenarios: &["public_v1_postgres_build_cancellation_and_retirement"],
        replacement_scenarios: &[
            "hung_build_peer_is_bounded_and_drained_before_successor_close",
            "hosted_public_v1_build_dispatches_exact_attempt_and_waits_for_matching_completion",
            "hosted_public_v1_removal_drains_build_excludes_peer_and_rejects_stale_session",
        ],
        expected: "build root and descendants drain before removal",
        eligibility: contract_gate,
    },
    Gate {
        id: "KSF-07",
        name: "swap_uses_captured_specific_quorum_capability",
        owner: "swap",
        postgres_scenarios: &[],
        replacement_scenarios: &[
            "hosted_public_v1_switchover_dispatches_exact_intent_and_accepts_matching_completion",
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
            "ambiguous_suffix_exactly_replays_quorum_peers_despite_higher_advertised_progress",
            "hosted_public_v1_switchover_dispatches_exact_intent_and_accepts_matching_completion",
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
            "production_config_has_no_protocol_selector_and_stateless_source_has_no_legacy_store_or_auth",
        ],
        expected: "stateless Public V1 is the sole built-in default",
        eligibility: replacement_ownership_gate,
    },
    Gate {
        id: "KSF-10",
        name: "replicator_open_owns_endpoint_lifetime",
        owner: "transport",
        postgres_scenarios: &[],
        replacement_scenarios: &[
            "public_v1_open_owns_registration_listener_and_writer_exactly_once",
        ],
        expected: "public endpoint lifetime follows Replicator lifecycle",
        eligibility: replacement_ownership_gate,
    },
    Gate {
        id: "KSF-11",
        name: "role_address_is_published_and_cleared",
        owner: "reporting",
        postgres_scenarios: &[],
        replacement_scenarios: &["hosted_public_v1_role_address_publishes_replaces_and_clears"],
        expected: "exact role completion publishes and clears address",
        eligibility: contract_gate,
    },
    Gate {
        id: "KSF-12",
        name: "supersession_cancels_and_drains_public_callbacks",
        owner: "operation",
        postgres_scenarios: &["public_v1_postgres_build_cancellation_and_retirement"],
        replacement_scenarios: &[
            "hosted_public_v1_supersession_cancels_build_root_and_drains_descendant_before_close",
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
            "hosted_public_v1_unreachable_survivor_blocks_access_until_return_or_retirement",
            "hosted_public_v1_rebuild_retires_and_restarts_fresh_replacement",
            "hosted_public_v1_switchover_dispatches_exact_intent_and_accepts_matching_completion",
        ],
        expected: "fault closes access and targets one exact incarnation",
        eligibility: contract_gate,
    },
    Gate {
        id: "KSF-14",
        name: "contained_close_failure_completes_outer_close",
        owner: "close",
        postgres_scenarios: &["public_v1_postgres_contained_close"],
        replacement_scenarios: &[
            "hosted_public_v1_contained_close_restarts_without_replaying_interrupted_children",
        ],
        expected: "child diagnostics do not change normalized outer close",
        eligibility: contract_gate,
    },
    Gate {
        id: "KSF-15",
        name: "callback_replay_follows_declared_disposition",
        owner: "replay",
        postgres_scenarios: &[],
        replacement_scenarios: &[
            "hosted_public_v1_contained_close_restarts_without_replaying_interrupted_children",
            "public_v1_replacement_schema_failed_build_containment_survives_reopen",
            "public_v1_replacement_schema_rebuild_then_cancellation_survives_reopen",
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
            "replication_ingress_accepts_only_exact_current_primary_in_three_replica_set",
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

fn scenario_filter(scenarios: fn(&Gate) -> &'static [&'static str]) -> String {
    GATES
        .iter()
        .flat_map(|gate| scenarios(gate).iter())
        .map(|scenario| format!("test({scenario})"))
        .collect::<Vec<_>>()
        .join(" or ")
}

fn run_scenarios(
    subject: &str,
    packages: &[&str],
    scenarios: fn(&Gate) -> &'static [&'static str],
    deadline: Instant,
) {
    let root = workspace_root();
    let filter = scenario_filter(scenarios);
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
    let mut list = Command::new("cargo");
    list.current_dir(&root).args(["nextest", "list"]);
    if let Some(archive) = &archive {
        list.arg("--archive-file").arg(archive);
    } else {
        for package in packages {
            list.args(["-p", package]);
        }
        list.arg("--all-features");
    }
    list.args(["--profile", "ordinary", "-E", &filter]);
    let list = bounded_output(&mut list, deadline, "list Public V1 conformance scenarios")
        .unwrap_or_else(|error| panic!("{error}"));
    assert!(
        list.status.success(),
        "replacement Public V1 conformance inventory failed: {}",
        String::from_utf8_lossy(&list.stderr)
    );
    let inventory = String::from_utf8(list.stdout).expect("nextest inventory is UTF-8");
    for gate in &GATES {
        for scenario in scenarios(gate) {
            assert!(
                inventory.contains(scenario),
                "{} scenario is not selected: {}",
                gate.id,
                scenario
            );
        }
    }

    let mut run = Command::new("cargo");
    run.current_dir(&root).args(["nextest", "run", "-j", "1"]);
    let extract = archive.as_ref().map(|archive| {
        let path = root.join("target").join(format!(
            "public-v1-conformance-extract-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("create Public V1 conformance extraction directory");
        run.arg("--archive-file")
            .arg(archive)
            .arg("--extract-to")
            .arg(&path)
            .arg("--workspace-remap")
            .arg(&root);
        path
    });
    if archive.is_none() {
        for package in packages {
            run.args(["-p", package]);
        }
        run.arg("--all-features");
    }
    run.args(["--profile", "ordinary", "-E", &filter]);
    let result = bounded_output(
        &mut run,
        deadline,
        &format!("run {subject} Public V1 conformance scenarios"),
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
}

#[cfg(all(test, unix))]
mod bounded_process_tests {
    use super::*;

    fn process_exists(pid: i32) -> bool {
        unsafe extern "C" {
            fn kill(pid: i32, signal: i32) -> i32;
        }
        unsafe { kill(pid, 0) == 0 }
    }

    #[test]
    fn shared_deadline_does_not_reset_and_reaps_nested_process_tree() {
        let root = workspace_root()
            .join("target")
            .join(format!("conformance-timeout-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let pid_file = root.join("descendant.pid");
        let deadline = Instant::now() + Duration::from_millis(400);
        let started = Instant::now();
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
            started.elapsed() < Duration::from_millis(550),
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
    assert_public_v1_conformance_subject(subject);
    assert_eq!(
        subject, "built-in-replacement",
        "only the built-in replacement subject has an executable nested gate"
    );
    let deadline = Instant::now() + CONFORMANCE_TIMEOUT;
    run_scenarios(
        "built-in-replacement",
        &["kuberic-runtime", "kuberic-controller"],
        |gate| gate.replacement_scenarios,
        deadline,
    );
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
