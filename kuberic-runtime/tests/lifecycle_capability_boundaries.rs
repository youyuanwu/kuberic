use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::PathBuf;

use quote::ToTokens;
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

fn transition_consumers(source: &str) -> Result<BTreeSet<String>, String> {
    let file = syn::parse_file(source).map_err(|error| format!("parse consumers: {error}"))?;
    let mut consumers = BTreeSet::new();
    for item in &file.items {
        match item {
            Item::Fn(function) if compact(&function.block).contains(".lifecycle()") => {
                consumers.insert(format!("fn {}", function.sig.ident));
            }
            Item::Impl(item) => {
                let owner = impl_name(item);
                for implementation in &item.items {
                    let ImplItem::Fn(method) = implementation else {
                        continue;
                    };
                    if compact(&method.block).contains(".lifecycle()") {
                        consumers.insert(format!("{owner}::{}", method.sig.ident));
                    }
                }
            }
            _ => {}
        }
    }
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

fn reject_broad_aliases(file: &syn::File) -> Result<(), String> {
    const FORBIDDEN: &[&str] = &[
        "LifecycleWiring",
        "ReplicatorLifecycleHost",
        "RuntimeHost",
        "PodRuntime",
        "ManagedLifecycleBackend",
        "CustomReplicatorHost",
    ];
    for item in &file.items {
        let Item::Type(alias) = item else {
            continue;
        };
        let target = compact(&alias.ty);
        if FORBIDDEN.iter().any(|forbidden| target.contains(forbidden)) {
            return Err(format!("broad lifecycle alias {} -> {target}", alias.ident));
        }
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
    let mut fixture_methods = BTreeSet::new();
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
                        fixture_methods.insert(method.sig.ident.to_string());
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
    if fixture_methods
        != names(&[
            "invalidate_public_access",
            "build_replica",
            "remove_replica",
        ])
    {
        return Err(format!(
            "fixture-gated capability methods changed: {fixture_methods:#?}"
        ));
    }
    reject_broad_aliases(&lifecycle_file)?;

    let custom_file =
        syn::parse_file(custom).map_err(|error| format!("parse custom host: {error}"))?;
    reject_broad_aliases(&custom_file)?;
    let expected_facade = names(&[
        "managed",
        "service",
        "registration",
        "is_managed",
        "restore_access",
        "admit_build_authority",
        "retire_build",
        "commit_access_transaction",
        "begin_access_effect",
        "wait_for_catch_up",
        "authorize_failover_prefix",
        "prepare_switchover",
        "refresh_progress",
        "observe_progress",
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
        "postcondition",
    ]);
    let mut facade_methods = None;
    for item in &custom_file.items {
        let Item::Impl(item) = item else {
            continue;
        };
        if item.trait_.is_some()
            || type_name(&item.self_ty).as_deref() != Some("ReplicatorLifecycleHost")
        {
            continue;
        }
        let methods = item
            .items
            .iter()
            .filter_map(|item| match item {
                ImplItem::Fn(method) => Some(method.sig.ident.to_string()),
                _ => None,
            })
            .collect();
        facade_methods = Some(methods);
    }
    if facade_methods.as_ref() != Some(&expected_facade) {
        return Err(format!(
            "transition facade allowance changed: {facade_methods:#?}"
        ));
    }
    let expected_facade_fields = BTreeMap::from([
        ("managed".to_owned(), "bool".to_owned()),
        ("access".to_owned(), "Arc<dynAccessLifecycle>".to_owned()),
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

    let hosting_file =
        syn::parse_file(hosting).map_err(|error| format!("parse hosting: {error}"))?;
    reject_broad_aliases(&hosting_file)?;
    let registered_fields = struct_fields(&hosting_file, "RegisteredReplicator")
        .ok_or_else(|| "RegisteredReplicator missing".to_owned())?;
    for (field, expected_type) in [
        ("process_lifecycle", "Option<lifecycle::ProcessRuntime>"),
        ("authority_lifecycle", "Option<lifecycle::AuthorityRuntime>"),
        ("peer_lifecycle", "Option<lifecycle::PeerRuntime>"),
        ("access_closure", "Option<lifecycle::AccessClosure>"),
        ("lifecycle_evidence", "Option<lifecycle::EvidenceRuntime>"),
    ] {
        if registered_fields.get(field).map(String::as_str) != Some(expected_type) {
            return Err(format!(
                "registered lifecycle field {field} missing or changed: {registered_fields:#?}"
            ));
        }
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

    let actual_consumers = transition_consumers(hosting)?
        .into_iter()
        .chain(transition_consumers(custom)?)
        .collect::<BTreeSet<_>>();
    let expected_consumers = names(&[
        "BuildQueueAdmission::new",
        "ExactBuildCancellation::new",
        "PodRuntime::authorize_build",
        "PodRuntime::build_generation",
        "PodRuntime::cancel_outbound_build",
        "PodRuntime::cancel_outbound_build_attempt",
        "PodRuntime::execute_admitted_build",
        "PodRuntime::next_outbound",
        "PodRuntime::observe_progress",
        "PodRuntime::observe_secondary_removal_witness",
        "PodRuntime::primary_replicator",
        "PodRuntime::reconcile_durable_access",
        "PodRuntime::reconstruct",
        "PodRuntime::reissue_outbound_build",
        "PodRuntime::restore_accepted_removal",
        "PodRuntime::testing_wait_for_catch_up",
        "PodRuntime::wait_for_build_completion",
        "RuntimeDataPlane::next_outbound",
        "RuntimeHost::consume_cancelled_build_effect",
        "RuntimeHost::lifecycle",
        "RuntimeHost::observe_build_completion",
        "RuntimeHost::prepare_effect",
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
    validate(&lifecycle, &custom, &hosting).unwrap();

    assert!(hosting.contains("#[path = \"lifecycle.rs\"]\nmod lifecycle;"));
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
}
