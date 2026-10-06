use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::PathBuf;

use quote::ToTokens;
use syn::visit::Visit;
use syn::{ImplItem, Item, TraitItem, Type};

fn source(path: &str) -> String {
    fs::read_to_string(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(path)).unwrap()
}

fn names(values: &[&str]) -> BTreeSet<String> {
    values.iter().map(|value| (*value).to_owned()).collect()
}

fn type_name(ty: &Type) -> Option<String> {
    let Type::Path(path) = ty else {
        return None;
    };
    path.path
        .segments
        .last()
        .map(|segment| segment.ident.to_string())
}

fn compact<T: ToTokens>(value: &T) -> String {
    value.to_token_stream().to_string().replace(' ', "")
}

fn impl_name(item: &syn::ItemImpl) -> String {
    type_name(&item.self_ty).unwrap_or_else(|| "<unknown>".into())
}

fn block_uses_transition_facade<T: ToTokens>(block: &T) -> bool {
    let block = compact(block);
    let mut rest = block.as_str();
    while let Some(index) = rest.find(".lifecycle") {
        let following = rest[index + ".lifecycle".len()..].chars().next();
        if following.is_none_or(|character| character != '_' && !character.is_ascii_alphanumeric())
        {
            return true;
        }
        rest = &rest[index + ".lifecycle".len()..];
    }
    false
}

fn collect_transition_consumers(items: &[Item], module: &str, consumers: &mut BTreeSet<String>) {
    for item in items {
        match item {
            Item::Fn(function) if block_uses_transition_facade(&function.block) => {
                consumers.insert(format!("{module}fn {}", function.sig.ident));
            }
            Item::Impl(item) => {
                let owner = impl_name(item);
                for implementation in &item.items {
                    let ImplItem::Fn(method) = implementation else {
                        continue;
                    };
                    if block_uses_transition_facade(&method.block) {
                        consumers.insert(format!("{module}{owner}::{}", method.sig.ident));
                    }
                }
            }
            Item::Mod(item) => {
                if let Some((_, items)) = &item.content {
                    collect_transition_consumers(
                        items,
                        &format!("{module}{}::", item.ident),
                        consumers,
                    );
                }
            }
            _ => {}
        }
    }
}

fn transition_consumers(source: &str) -> Result<BTreeSet<String>, String> {
    let file = syn::parse_file(source).map_err(|error| format!("parse consumers: {error}"))?;
    let mut consumers = BTreeSet::new();
    collect_transition_consumers(&file.items, "", &mut consumers);
    Ok(consumers)
}

fn struct_fields(file: &syn::File, name: &str) -> Option<BTreeMap<String, String>> {
    for item in &file.items {
        let Item::Struct(item) = item else {
            continue;
        };
        if item.ident != name {
            continue;
        }
        return Some(
            item.fields
                .iter()
                .filter_map(|field| {
                    field
                        .ident
                        .as_ref()
                        .map(|ident| (ident.to_string(), compact(&field.ty)))
                })
                .collect(),
        );
    }
    None
}

struct BroadAliasVisitor {
    issue: Option<String>,
}

impl<'ast> Visit<'ast> for BroadAliasVisitor {
    fn visit_item_type(&mut self, alias: &'ast syn::ItemType) {
        const FORBIDDEN: &[&str] = &[
            "LifecycleWiring",
            "ReplicatorLifecycleHost",
            "RuntimeHost",
            "PodRuntime",
            "ManagedLifecycleBackend",
            "CustomReplicatorHost",
        ];
        let target = compact(&alias.ty);
        if FORBIDDEN.iter().any(|forbidden| target.contains(forbidden)) {
            self.issue = Some(format!("broad lifecycle alias {} -> {target}", alias.ident));
        }
        syn::visit::visit_item_type(self, alias);
    }
}

fn reject_broad_aliases(file: &syn::File) -> Result<(), String> {
    let mut visitor = BroadAliasVisitor { issue: None };
    visitor.visit_file(file);
    match visitor.issue {
        Some(issue) => Err(issue),
        None => Ok(()),
    }
}

fn impl_methods(file: &syn::File, name: &str) -> Option<BTreeSet<String>> {
    let mut found = false;
    let mut methods = BTreeSet::new();
    for item in &file.items {
        let Item::Impl(item) = item else {
            continue;
        };
        if item.trait_.is_some() || type_name(&item.self_ty).as_deref() != Some(name) {
            continue;
        }
        found = true;
        methods.extend(item.items.iter().filter_map(|item| match item {
            ImplItem::Fn(method) => Some(method.sig.ident.to_string()),
            _ => None,
        }));
    }
    found.then_some(methods)
}

fn impl_method<'a>(file: &'a syn::File, owner: &str, method: &str) -> Option<&'a syn::ImplItemFn> {
    for item in &file.items {
        let Item::Impl(item) = item else {
            continue;
        };
        if item.trait_.is_some() || type_name(&item.self_ty).as_deref() != Some(owner) {
            continue;
        }
        for implementation in &item.items {
            let ImplItem::Fn(implementation) = implementation else {
                continue;
            };
            if implementation.sig.ident == method {
                return Some(implementation);
            }
        }
    }
    None
}

fn assert_shape(
    file: &syn::File,
    name: &str,
    expected_fields: &[(&str, &str)],
    expected_methods: &[&str],
) -> Result<(), String> {
    let expected_fields = expected_fields
        .iter()
        .map(|(field, ty)| ((*field).to_owned(), (*ty).to_owned()))
        .collect::<BTreeMap<_, _>>();
    if struct_fields(file, name).as_ref() != Some(&expected_fields) {
        return Err(format!("{name} field budget changed"));
    }
    let expected_methods = names(expected_methods);
    if impl_methods(file, name).as_ref() != Some(&expected_methods) {
        return Err(format!("{name} method budget changed"));
    }
    Ok(())
}

fn validate(lifecycle: &str, custom: &str, hosting: &str) -> Result<(), String> {
    for source in [lifecycle, custom, hosting] {
        if source.contains("ReplicatorLifecycleBackend") {
            return Err("universal lifecycle backend remains".into());
        }
    }

    let lifecycle_file =
        syn::parse_file(lifecycle).map_err(|error| format!("parse lifecycle: {error}"))?;
    let expected_traits = BTreeMap::from([
        (
            "ProcessLifecycle",
            names(&[
                "owns_stream_session",
                "complete_open",
                "complete_close",
                "complete_abort",
                "notify_abort",
                "fence_writes",
                "invalidate_public_access",
                "settle_primary_prefix",
            ]),
        ),
        (
            "AuthorityLifecycle",
            names(&[
                "cancel_configuration_work",
                "restore_authority",
                "admit_authority",
                "register_peer_session",
                "describe_peer",
            ]),
        ),
        (
            "AccessLifecycle",
            names(&[
                "defer_restored_access",
                "run_access_transaction",
                "restored_access",
            ]),
        ),
        (
            "BuildLifecycle",
            names(&[
                "admit_build_authority",
                "retire_build",
                "cancel_outbound_build",
                "cancel_outbound_build_attempt",
                "build_generation",
                "wait_for_build_completion",
                "build_replica",
                "remove_replica",
                "select_build",
                "execute_build",
                "accept_build",
                "enqueue_build",
                "confirm_build_completion",
            ]),
        ),
        (
            "TopologyLifecycle",
            names(&[
                "wait_for_catch_up",
                "authorize_failover_prefix",
                "prepare_switchover",
                "prepare_secondary_removal",
                "observe_secondary_removal",
                "observe_secondary_removal_progress",
                "accept_secondary_removal",
                "accept_historical_secondary_removal",
                "fence_retirement",
                "complete_retirement",
                "topology_receipt",
            ]),
        ),
        (
            "LifecycleObservation",
            names(&[
                "refresh_progress",
                "observe_progress",
                "snapshot",
                "postcondition",
            ]),
        ),
        ("OutboundLifecycle", names(&["next_outbound"])),
    ])
    .into_iter()
    .map(|(name, methods)| (name.to_owned(), methods))
    .collect::<BTreeMap<_, _>>();
    let mut actual_traits = BTreeMap::new();
    let mut fixture_methods = BTreeMap::new();
    for item in &lifecycle_file.items {
        let Item::Trait(item) = item else {
            continue;
        };
        let bounds = item
            .supertraits
            .to_token_stream()
            .to_string()
            .replace(' ', "");
        if bounds != "Send+Sync" {
            return Err(format!(
                "{} has unsupported supertraits: {bounds}",
                item.ident
            ));
        }
        let methods = item
            .items
            .iter()
            .filter_map(|item| match item {
                TraitItem::Fn(method) => {
                    if method
                        .attrs
                        .iter()
                        .any(|attribute| attribute.path().is_ident("cfg"))
                    {
                        let cfg = method
                            .attrs
                            .iter()
                            .find(|attribute| attribute.path().is_ident("cfg"))
                            .map(compact)
                            .unwrap();
                        fixture_methods.insert(method.sig.ident.to_string(), cfg);
                    }
                    Some(method.sig.ident.to_string())
                }
                _ => None,
            })
            .collect();
        actual_traits.insert(item.ident.to_string(), methods);
    }
    if actual_traits != expected_traits {
        return Err(format!(
            "capability method budgets changed: {actual_traits:#?}"
        ));
    }
    let fixture_cfg = "#[cfg(any(all(test,kuberic_workspace_tests),feature=\"testing\"))]";
    let expected_fixture_methods = BTreeMap::from([
        ("build_replica".to_owned(), fixture_cfg.to_owned()),
        (
            "invalidate_public_access".to_owned(),
            fixture_cfg.to_owned(),
        ),
        ("remove_replica".to_owned(), fixture_cfg.to_owned()),
    ]);
    if fixture_methods != expected_fixture_methods {
        return Err(format!(
            "fixture-gated capability methods changed: {fixture_methods:#?}"
        ));
    }
    reject_broad_aliases(&lifecycle_file)?;
    assert_shape(
        &lifecycle_file,
        "LifecycleWiring",
        &[
            ("process", "Arc<dynProcessLifecycle>"),
            ("authority", "Arc<dynAuthorityLifecycle>"),
            ("access", "Arc<dynAccessLifecycle>"),
            ("build", "Arc<dynBuildLifecycle>"),
            ("topology", "Arc<dynTopologyLifecycle>"),
            ("observation", "Arc<dynLifecycleObservation>"),
            ("outbound", "Arc<dynOutboundLifecycle>"),
        ],
        &[
            "new",
            "process_runtime",
            "authority_runtime",
            "peer_runtime",
            "access_closure",
            "access_runtime",
            "report_lifecycle",
            "evidence_runtime",
            "effect_evidence_runtime",
        ],
    )?;
    assert_shape(
        &lifecycle_file,
        "ProcessRuntime",
        &[("inner", "Arc<dynProcessLifecycle>")],
        &[
            "owns_stream_session",
            "complete_open",
            "complete_close",
            "complete_abort",
            "notify_abort",
            "fence_writes",
            "invalidate_public_access",
            "settle_primary_prefix",
        ],
    )?;
    assert_shape(
        &lifecycle_file,
        "AuthorityRuntime",
        &[("inner", "Arc<dynAuthorityLifecycle>")],
        &[
            "cancel_configuration_work",
            "restore_authority",
            "admit_authority",
        ],
    )?;
    assert_shape(
        &lifecycle_file,
        "PeerRuntime",
        &[("inner", "Arc<dynAuthorityLifecycle>")],
        &["register_peer_session", "describe_peer"],
    )?;
    assert_shape(
        &lifecycle_file,
        "AccessClosure",
        &[("inner", "Arc<dynAccessLifecycle>")],
        &["set_access"],
    )?;
    assert_shape(
        &lifecycle_file,
        "EvidenceRuntime",
        &[("inner", "Arc<dynLifecycleObservation>")],
        &["snapshot"],
    )?;
    assert_shape(
        &lifecycle_file,
        "AccessRuntime",
        &[("inner", "Arc<dynAccessLifecycle>")],
        &["begin_effect", "restore"],
    )?;
    assert_shape(
        &lifecycle_file,
        "ReportLifecycle",
        &[
            ("access", "Arc<dynAccessLifecycle>"),
            ("observation", "Arc<dynLifecycleObservation>"),
        ],
        &["observe_progress", "reconcile_access", "snapshot"],
    )?;
    assert_shape(
        &lifecycle_file,
        "EffectEvidenceRuntime",
        &[("inner", "Arc<dynLifecycleObservation>")],
        &["refresh_progress", "postcondition"],
    )?;

    let custom_file =
        syn::parse_file(custom).map_err(|error| format!("parse custom host: {error}"))?;
    reject_broad_aliases(&custom_file)?;
    let expected_facade = names(&[
        "managed",
        "service",
        "registration",
        "is_managed",
        "admit_build_authority",
        "retire_build",
        "wait_for_catch_up",
        "authorize_failover_prefix",
        "prepare_switchover",
        "prepare_secondary_removal",
        "observe_secondary_removal",
        "observe_secondary_removal_progress",
        "accept_secondary_removal",
        "accept_historical_secondary_removal",
        "fence_retirement",
        "complete_retirement",
        "snapshot",
        "cancel_outbound_build",
        "cancel_outbound_build_attempt",
        "build_generation",
        "next_outbound",
        "wait_for_build_completion",
        "build_replica",
        "remove_replica",
        "select_build",
        "execute_build",
        "accept_build",
        "enqueue_build",
        "topology_receipt",
        "confirm_build_completion",
    ]);
    let facade_methods = impl_methods(&custom_file, "ReplicatorLifecycleHost");
    if facade_methods.as_ref() != Some(&expected_facade) {
        return Err(format!(
            "transition facade allowance changed: {facade_methods:#?}"
        ));
    }
    let expected_facade_fields = BTreeMap::from([
        ("managed".to_owned(), "bool".to_owned()),
        ("build".to_owned(), "Arc<dynBuildLifecycle>".to_owned()),
        (
            "topology".to_owned(),
            "Arc<dynTopologyLifecycle>".to_owned(),
        ),
        (
            "observation".to_owned(),
            "Arc<dynLifecycleObservation>".to_owned(),
        ),
        (
            "outbound".to_owned(),
            "Arc<dynOutboundLifecycle>".to_owned(),
        ),
    ]);
    if struct_fields(&custom_file, "ReplicatorLifecycleHost").as_ref()
        != Some(&expected_facade_fields)
    {
        return Err("transition facade retains unapproved capabilities".into());
    }
    let expected_registration_fields = BTreeMap::from([
        (
            "lifecycle".to_owned(),
            "Arc<ReplicatorLifecycleHost>".to_owned(),
        ),
        (
            "process".to_owned(),
            "super::lifecycle::ProcessRuntime".to_owned(),
        ),
        (
            "authority".to_owned(),
            "super::lifecycle::AuthorityRuntime".to_owned(),
        ),
        (
            "peer".to_owned(),
            "super::lifecycle::PeerRuntime".to_owned(),
        ),
        (
            "access_closure".to_owned(),
            "super::lifecycle::AccessClosure".to_owned(),
        ),
        (
            "access".to_owned(),
            "super::lifecycle::AccessRuntime".to_owned(),
        ),
        (
            "report".to_owned(),
            "super::lifecycle::ReportLifecycle".to_owned(),
        ),
        (
            "evidence".to_owned(),
            "super::lifecycle::EvidenceRuntime".to_owned(),
        ),
        (
            "effect_evidence".to_owned(),
            "super::lifecycle::EffectEvidenceRuntime".to_owned(),
        ),
    ]);
    if struct_fields(&custom_file, "ReplicatorLifecycleRegistration").as_ref()
        != Some(&expected_registration_fields)
    {
        return Err("lifecycle registration retained complete wiring".into());
    }
    let expected_queue_fields = BTreeMap::from([
        ("generation".to_owned(), "u64".to_owned()),
        (
            "decision".to_owned(),
            "Option<oneshot::Sender<bool>>".to_owned(),
        ),
        (
            "completion".to_owned(),
            "tokio::task::JoinHandle<()>".to_owned(),
        ),
    ]);
    if struct_fields(&custom_file, "BuildQueueAdmission").as_ref() != Some(&expected_queue_fields) {
        return Err("build queue cancellation capture changed".into());
    }
    let queue_cancellation = impl_method(&custom_file, "BuildQueueAdmission", "new")
        .ok_or_else(|| "BuildQueueAdmission::new missing".to_owned())?;
    if !compact(&queue_cancellation.sig).contains("host:Weak<RuntimeHost>") {
        return Err("build queue cancellation no longer receives a weak host".into());
    }

    let hosting_file =
        syn::parse_file(hosting).map_err(|error| format!("parse hosting: {error}"))?;
    reject_broad_aliases(&hosting_file)?;
    let report_host = hosting_file
        .items
        .iter()
        .find_map(|item| match item {
            Item::Trait(item) if item.ident == "ReportHost" => Some(item),
            _ => None,
        })
        .ok_or_else(|| "ReportHost missing".to_owned())?;
    if compact(&report_host.supertraits) != "Send+Sync"
        || report_host
            .items
            .iter()
            .filter_map(|item| match item {
                TraitItem::Fn(method) => Some(method.sig.ident.to_string()),
                _ => None,
            })
            .collect::<BTreeSet<_>>()
            != names(&[
                "observe_progress",
                "snapshot",
                "reconcile_access",
                "partition_report",
                "catch_up_capability",
            ])
    {
        return Err("report host capability budget changed".into());
    }
    assert_shape(
        &hosting_file,
        "ReportRuntime",
        &[("inner", "Arc<dynReportHost>")],
        &[
            "observe_progress",
            "snapshot",
            "reconcile_durable_access",
            "partition_report",
            "catch_up_capability",
        ],
    )?;
    let registered_fields = struct_fields(&hosting_file, "RegisteredReplicator")
        .ok_or_else(|| "RegisteredReplicator missing".to_owned())?;
    let expected_registered_fields = BTreeMap::from([
        ("control".to_owned(), "Arc<dynReplicator>".to_owned()),
        (
            "primary".to_owned(),
            "Option<Arc<dynPrimaryReplicator>>".to_owned(),
        ),
        (
            "provider".to_owned(),
            "Option<Arc<dynStateProvider>>".to_owned(),
        ),
        (
            "lifecycle".to_owned(),
            "Option<Arc<custom::ReplicatorLifecycleHost>>".to_owned(),
        ),
        (
            "process_lifecycle".to_owned(),
            "Option<lifecycle::ProcessRuntime>".to_owned(),
        ),
        (
            "authority_lifecycle".to_owned(),
            "Option<lifecycle::AuthorityRuntime>".to_owned(),
        ),
        (
            "peer_lifecycle".to_owned(),
            "Option<lifecycle::PeerRuntime>".to_owned(),
        ),
        (
            "access_closure".to_owned(),
            "Option<lifecycle::AccessClosure>".to_owned(),
        ),
        (
            "access_lifecycle".to_owned(),
            "Option<lifecycle::AccessRuntime>".to_owned(),
        ),
        (
            "report_lifecycle".to_owned(),
            "Option<lifecycle::ReportLifecycle>".to_owned(),
        ),
        (
            "lifecycle_evidence".to_owned(),
            "Option<lifecycle::EvidenceRuntime>".to_owned(),
        ),
        (
            "effect_evidence".to_owned(),
            "Option<lifecycle::EffectEvidenceRuntime>".to_owned(),
        ),
        (
            "managed_data_plane".to_owned(),
            "Option<Arc<dynManagedReplicatorDataPlane>>".to_owned(),
        ),
    ]);
    if registered_fields != expected_registered_fields {
        return Err(format!(
            "registered lifecycle field inventory changed: {registered_fields:#?}"
        ));
    }
    let expected_cancellation_fields = BTreeMap::from([
        (
            "decision".to_owned(),
            "Option<tokio::sync::oneshot::Sender<BuildCancellationDecision>>".to_owned(),
        ),
        (
            "completion".to_owned(),
            "tokio::task::JoinHandle<Result<()>>".to_owned(),
        ),
    ]);
    if struct_fields(&hosting_file, "ExactBuildCancellation").as_ref()
        != Some(&expected_cancellation_fields)
    {
        return Err("exact build cancellation capture changed".into());
    }
    let exact_cancellation = impl_method(&hosting_file, "ExactBuildCancellation", "new")
        .ok_or_else(|| "ExactBuildCancellation::new missing".to_owned())?;
    let cancellation_body = compact(&exact_cancellation.block);
    if cancellation_body.matches("Arc::downgrade(host)").count() != 1
        || cancellation_body.contains("Arc::clone(host)")
        || cancellation_body.contains("host.clone()")
    {
        return Err("exact build cancellation captured a broad host owner".into());
    }

    let actual_consumers = transition_consumers(hosting)?
        .into_iter()
        .chain(transition_consumers(custom)?)
        .collect::<BTreeSet<_>>();
    let expected_consumers = names(&[
        "BuildQueueAdmission::new",
        "ExactBuildCancellation::new",
        "HostedPrimaryReplicator::build_replica",
        "HostedPrimaryReplicator::remove_replica",
        "PodRuntime::authorize_build",
        "PodRuntime::build_generation",
        "PodRuntime::cancel_outbound_build",
        "PodRuntime::cancel_outbound_build_attempt",
        "PodRuntime::execute_admitted_build",
        "PodRuntime::next_outbound",
        "PodRuntime::observe_secondary_removal_witness",
        "PodRuntime::primary_replicator",
        "PodRuntime::reconstruct",
        "PodRuntime::reissue_outbound_build",
        "PodRuntime::restore_accepted_removal",
        "PodRuntime::testing_wait_for_catch_up",
        "PodRuntime::wait_for_build_completion",
        "RegisteredReplicator::lifecycle",
        "RuntimeDataPlane::next_outbound",
        "RuntimeHost::consume_cancelled_build_effect",
        "RuntimeHost::lifecycle",
        "RuntimeHost::observe_build_completion",
        "RuntimeHost::prepare_effect",
        "RuntimeHost::register_interfaces",
        "RuntimeHost::snapshot",
    ]);
    if actual_consumers != expected_consumers {
        return Err(format!(
            "transition facade consumer inventory changed: {actual_consumers:#?}"
        ));
    }

    Ok(())
}

#[test]
fn lifecycle_capabilities_have_bounded_method_and_consumer_budgets() {
    let lifecycle = source("src/host/lifecycle.rs");
    let custom = source("src/host/custom.rs");
    let hosting = source("src/host/hosting.rs");
    let report = source("src/host/report.rs");
    validate(&lifecycle, &custom, &hosting).unwrap();

    assert!(hosting.contains("#[path = \"lifecycle.rs\"]\nmod lifecycle;"));
    assert!(
        report.contains("use crate::host::hosting::ReportRuntime;")
            && !report.contains("use crate::host::hosting::PodRuntime;")
            && report.contains("report(&self, runtime: &ReportRuntime)")
    );
    let host_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/host");
    for entry in fs::read_dir(&host_root).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().and_then(|extension| extension.to_str()) != Some("rs")
            || matches!(
                path.file_name().and_then(|name| name.to_str()),
                Some("hosting.rs" | "custom.rs")
            )
        {
            continue;
        }
        let source = fs::read_to_string(&path).unwrap();
        assert!(
            !source.contains("ReplicatorLifecycleHost"),
            "{} introduced an unclassified transition-facade consumer",
            path.display()
        );
    }
}

#[test]
fn lifecycle_capability_guard_rejects_broadening_mutations() {
    let lifecycle = source("src/host/lifecycle.rs");
    let custom = source("src/host/custom.rs");
    let hosting = source("src/host/hosting.rs");

    let broad_process = lifecycle.replace(
        "    async fn settle_primary_prefix(&self) -> Result<()>;\n",
        "    async fn settle_primary_prefix(&self) -> Result<()>;\n    async fn execute_build(&self, replica: ReplicaInformation) -> Result<Option<BuildAdmission>>;\n",
    );
    assert!(validate(&broad_process, &custom, &hosting).is_err());

    let universal =
        format!("{lifecycle}\ntrait UniversalLifecycle: ProcessLifecycle + BuildLifecycle {{}}\n");
    assert!(validate(&universal, &custom, &hosting).is_err());

    let broad_facade = custom.replace(
        "impl ReplicatorLifecycleHost {\n",
        "impl ReplicatorLifecycleHost {\n    async fn admit_authority(&self, authority: AdmittedAuthority) -> Result<()> { self.wiring.authority.admit_authority(authority).await }\n",
    );
    assert!(validate(&lifecycle, &broad_facade, &hosting).is_err());

    let missing_peer_view =
        hosting.replace("    peer_lifecycle: Option<lifecycle::PeerRuntime>,\n", "");
    assert!(validate(&lifecycle, &custom, &missing_peer_view).is_err());

    let broad_alias = format!("{lifecycle}\ntype Everything = LifecycleWiring;\n");
    assert!(validate(&broad_alias, &custom, &hosting).is_err());

    let universal_getter = custom.replace(
        "impl ReplicatorLifecycleHost {\n",
        "impl ReplicatorLifecycleHost {\n    fn all_capabilities(&self) -> &LifecycleWiring { unreachable!() }\n",
    );
    assert!(validate(&lifecycle, &universal_getter, &hosting).is_err());

    let broad_cancellation = hosting.replace(
        "struct ExactBuildCancellation {\n",
        "struct ExactBuildCancellation {\n    host: Weak<RuntimeHost>,\n",
    );
    assert!(validate(&lifecycle, &custom, &broad_cancellation).is_err());

    let new_facade_consumer = hosting.replace(
        "impl PodRuntime {\n",
        "impl PodRuntime {\n    async fn leaked_lifecycle(&self) { let _ = self.host.lifecycle(); }\n",
    );
    assert!(validate(&lifecycle, &custom, &new_facade_consumer).is_err());

    let universal_view_getter = lifecycle.replace(
        "impl ProcessRuntime {\n",
        "impl ProcessRuntime {\n    fn build_capability(&self) -> Arc<dyn BuildLifecycle> { unreachable!() }\n",
    );
    assert!(validate(&universal_view_getter, &custom, &hosting).is_err());

    let broad_view_field = lifecycle.replace(
        "pub(super) struct ProcessRuntime {\n",
        "pub(super) struct ProcessRuntime {\n    build: Arc<dyn BuildLifecycle>,\n",
    );
    assert!(validate(&broad_view_field, &custom, &hosting).is_err());

    let local_alias = lifecycle.replace(
        "    pub(super) async fn set_access(&self, read: AccessStatus, write: AccessStatus) -> Result<()> {\n",
        "    pub(super) async fn set_access(&self, read: AccessStatus, write: AccessStatus) -> Result<()> {\n        type AllCapabilities = LifecycleWiring;\n",
    );
    assert!(validate(&local_alias, &custom, &hosting).is_err());

    let always_enabled_fixture = lifecycle.replacen(
        "#[cfg(any(all(test, kuberic_workspace_tests), feature = \"testing\"))]",
        "#[cfg(all())]",
        1,
    );
    assert!(validate(&always_enabled_fixture, &custom, &hosting).is_err());

    let nested_consumer = format!(
        "{hosting}\nmod leaked {{ async fn consume(host: &RuntimeHost) {{ let _ = host.lifecycle(); }} }}\n"
    );
    assert!(validate(&lifecycle, &custom, &nested_consumer).is_err());

    let direct_field_consumer = hosting.replace(
        "impl PodRuntime {\n",
        "impl PodRuntime {\n    async fn leaked_field(&self) { let registered = self.host.registered.get().unwrap(); let _ = registered.lifecycle.snapshot().await; }\n",
    );
    assert!(validate(&lifecycle, &custom, &direct_field_consumer).is_err());

    let broad_closure_capture = hosting.replace(
        "        let host = Arc::downgrade(host);\n",
        "        let broad_host = host.clone();\n        let host = Arc::downgrade(host);\n",
    );
    assert!(validate(&lifecycle, &custom, &broad_closure_capture).is_err());

    let retained_wiring = custom.replace(
        "pub(super) struct ReplicatorLifecycleRegistration {\n",
        "pub(super) struct ReplicatorLifecycleRegistration {\n    pub(super) wiring: LifecycleWiring,\n",
    );
    assert!(validate(&lifecycle, &retained_wiring, &hosting).is_err());

    let second_view_impl = format!(
        "{lifecycle}\nimpl ProcessRuntime {{ fn build_capability(&self) -> Arc<dyn BuildLifecycle> {{ unreachable!() }} }}\n"
    );
    assert!(validate(&second_view_impl, &custom, &hosting).is_err());

    let qualified_local_alias = lifecycle.replace(
        "    pub(super) async fn set_access(&self, read: AccessStatus, write: AccessStatus) -> Result<()> {\n",
        "    pub(super) async fn set_access(&self, read: AccessStatus, write: AccessStatus) -> Result<()> {\n        type AllCapabilities = super::lifecycle::LifecycleWiring;\n",
    );
    assert!(validate(&qualified_local_alias, &custom, &hosting).is_err());

    let borrowed_facade_field = hosting.replace(
        "impl PodRuntime {\n",
        "impl PodRuntime {\n    fn leaked_field(&self) { let registered = self.host.registered.get().unwrap(); let _borrowed = &registered.lifecycle; }\n",
    );
    assert!(validate(&lifecycle, &custom, &borrowed_facade_field).is_err());
}
