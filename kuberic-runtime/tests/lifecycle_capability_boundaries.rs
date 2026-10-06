use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::PathBuf;

use quote::ToTokens;
use syn::visit::Visit;
use syn::{ImplItem, Item, TraitItem, Type};

const CAPABILITY_MARKERS: &[&str] = &[
    "ProcessRuntime",
    "AuthorityRuntime",
    "PeerRuntime",
    "AccessClosure",
    "AccessRuntime",
    "ReportLifecycle",
    "EvidenceRuntime",
    "EffectEvidenceRuntime",
    "BuildLifecycleRuntime",
    "BuildCancellationRuntime",
    "OutboundLifecycleRuntime",
    "RemovalWitnessRuntime",
    "TopologyRuntime",
    "RecoveryRuntime",
    "ReportRuntime",
    "BuildRuntime",
    "BuildAttemptRuntime",
    "PeerDiscoveryRuntime",
    "OutboundRuntime",
    "dynProcessLifecycle",
    "dynAuthorityLifecycle",
    "dynAccessLifecycle",
    "dynBuildLifecycle",
    "dynBuildCancellation",
    "dynTopologyLifecycle",
    "dynLifecycleObservation",
    "dynOutboundLifecycle",
];

fn capability_marker_count(value: &str) -> usize {
    let identifiers = value
        .split(|character: char| !(character.is_ascii_alphanumeric() || character == '_'))
        .filter(|identifier| !identifier.is_empty())
        .collect::<BTreeSet<_>>();
    CAPABILITY_MARKERS
        .iter()
        .filter(|marker| identifiers.contains(**marker))
        .count()
}

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
        } else if capability_marker_count(&target) >= 2 {
            self.issue = Some(format!(
                "capability aggregate alias {} -> {target}",
                alias.ident
            ));
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

fn reject_capability_escape_paths(
    file: &syn::File,
    traits: &[&str],
    views: &[&str],
) -> Result<(), String> {
    const FORBIDDEN: &[&str] = &[
        "LifecycleWiring",
        "ReplicatorLifecycleHost",
        "RegisteredReplicator",
        "RuntimeHost",
        "PodRuntime",
        "ManagedLifecycleBackend",
        "CustomReplicatorHost",
    ];
    for item in &file.items {
        match item {
            Item::Trait(item) if traits.contains(&item.ident.to_string().as_str()) => {
                for method in &item.items {
                    let TraitItem::Fn(method) = method else {
                        continue;
                    };
                    let signature = compact(&method.sig);
                    if FORBIDDEN
                        .iter()
                        .any(|forbidden| signature.contains(forbidden))
                    {
                        return Err(format!(
                            "{}::{} exposes a broad type",
                            item.ident, method.sig.ident
                        ));
                    }
                }
            }
            Item::Impl(item)
                if type_name(&item.self_ty)
                    .as_deref()
                    .is_some_and(|name| views.contains(&name)) =>
            {
                if item.trait_.is_some() {
                    return Err(format!(
                        "{} implements an unapproved conversion trait",
                        impl_name(item)
                    ));
                }
                for method in &item.items {
                    let ImplItem::Fn(method) = method else {
                        continue;
                    };
                    let signature = compact(&method.sig);
                    if FORBIDDEN
                        .iter()
                        .any(|forbidden| signature.contains(forbidden))
                    {
                        return Err(format!(
                            "{}::{} exposes a broad type",
                            impl_name(item),
                            method.sig.ident
                        ));
                    }
                }
            }
            _ => {}
        }
    }
    Ok(())
}

fn reject_unclassified_capability_aggregates(
    file: &syn::File,
    allowed: &[&str],
) -> Result<(), String> {
    struct AggregateVisitor<'a> {
        allowed: &'a [&'a str],
        depth: usize,
        issue: Option<String>,
    }

    impl<'ast> Visit<'ast> for AggregateVisitor<'_> {
        fn visit_item_struct(&mut self, item: &'ast syn::ItemStruct) {
            let capabilities = item
                .fields
                .iter()
                .map(|field| capability_marker_count(&compact(&field.ty)))
                .sum::<usize>();
            if capabilities >= 2
                && (self.depth > 0 || !self.allowed.contains(&item.ident.to_string().as_str()))
            {
                self.issue = Some(format!(
                    "{} is an unclassified capability aggregate",
                    item.ident
                ));
            }
            syn::visit::visit_item_struct(self, item);
        }

        fn visit_block(&mut self, block: &'ast syn::Block) {
            self.depth += 1;
            syn::visit::visit_block(self, block);
            self.depth -= 1;
        }

        fn visit_item_mod(&mut self, item: &'ast syn::ItemMod) {
            self.depth += 1;
            syn::visit::visit_item_mod(self, item);
            self.depth -= 1;
        }
    }

    let mut visitor = AggregateVisitor {
        allowed,
        depth: 0,
        issue: None,
    };
    visitor.visit_file(file);
    match visitor.issue {
        Some(issue) => Err(issue),
        None => Ok(()),
    }
}

fn reject_aggregate_signatures(file: &syn::File) -> Result<(), String> {
    fn check_signature(owner: &str, signature: &syn::Signature) -> Result<(), String> {
        let signature = compact(signature);
        if capability_marker_count(&signature) >= 2 {
            return Err(format!("{owner} exposes a capability aggregate signature"));
        }
        Ok(())
    }

    fn inspect(items: &[Item], module: &str) -> Result<(), String> {
        for item in items {
            match item {
                Item::Fn(item) => {
                    check_signature(&format!("{module}fn {}", item.sig.ident), &item.sig)?
                }
                Item::Trait(item) => {
                    for method in &item.items {
                        if let TraitItem::Fn(method) = method {
                            check_signature(
                                &format!("{module}{}::{}", item.ident, method.sig.ident),
                                &method.sig,
                            )?;
                        }
                    }
                }
                Item::Impl(item) => {
                    for method in &item.items {
                        if let ImplItem::Fn(method) = method {
                            check_signature(
                                &format!("{module}{}::{}", impl_name(item), method.sig.ident),
                                &method.sig,
                            )?;
                        }
                    }
                }
                Item::Mod(item) => {
                    if let Some((_, items)) = &item.content {
                        inspect(items, &format!("{module}{}::", item.ident))?;
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }

    inspect(&file.items, "")
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
            "BuildCancellation",
            names(&[
                "cancel_outbound_build",
                "cancel_outbound_build_attempt",
                "build_generation",
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
            ("build_cancellation", "Arc<dynBuildCancellation>"),
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
            "build_runtime",
            "build_cancellation",
            "outbound_runtime",
            "removal_witness_runtime",
            "topology_runtime",
            "recovery_runtime",
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
        &["cancel_configuration_work", "admit_authority"],
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
        &["begin_effect"],
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
    assert_shape(
        &lifecycle_file,
        "BuildLifecycleRuntime",
        &[
            ("inner", "Arc<dynBuildLifecycle>"),
            ("cancellation", "Arc<dynBuildCancellation>"),
            ("managed", "bool"),
        ],
        &[
            "is_managed",
            "admit_authority",
            "retire",
            "cancel",
            "wait_for_completion",
            "build_replica",
            "remove_replica",
            "select",
            "execute",
            "accept",
            "enqueue",
            "confirm",
        ],
    )?;
    assert_shape(
        &lifecycle_file,
        "BuildCancellationRuntime",
        &[("inner", "Arc<dynBuildCancellation>")],
        &["generation", "cancel_attempt"],
    )?;
    assert_shape(
        &lifecycle_file,
        "OutboundLifecycleRuntime",
        &[("inner", "Arc<dynOutboundLifecycle>")],
        &["next"],
    )?;
    assert_shape(
        &lifecycle_file,
        "RemovalWitnessRuntime",
        &[("inner", "Arc<dynTopologyLifecycle>")],
        &["observe"],
    )?;
    assert_shape(
        &lifecycle_file,
        "TopologyRuntime",
        &[("inner", "Arc<dynTopologyLifecycle>")],
        &[
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
            "receipt",
        ],
    )?;
    assert_shape(
        &lifecycle_file,
        "RecoveryRuntime",
        &[
            ("authority", "Arc<dynAuthorityLifecycle>"),
            ("access", "Arc<dynAccessLifecycle>"),
            ("topology", "Arc<dynTopologyLifecycle>"),
            ("observation", "Arc<dynLifecycleObservation>"),
        ],
        &[
            "restore_authority",
            "snapshot",
            "restore_access",
            "accept_secondary_removal",
            "accept_historical_secondary_removal",
        ],
    )?;
    reject_capability_escape_paths(
        &lifecycle_file,
        &[
            "ProcessLifecycle",
            "AuthorityLifecycle",
            "AccessLifecycle",
            "BuildLifecycle",
            "BuildCancellation",
            "TopologyLifecycle",
            "LifecycleObservation",
            "OutboundLifecycle",
        ],
        &[
            "ProcessRuntime",
            "AuthorityRuntime",
            "PeerRuntime",
            "AccessClosure",
            "AccessRuntime",
            "ReportLifecycle",
            "EvidenceRuntime",
            "EffectEvidenceRuntime",
            "BuildLifecycleRuntime",
            "BuildCancellationRuntime",
            "OutboundLifecycleRuntime",
            "RemovalWitnessRuntime",
            "TopologyRuntime",
            "RecoveryRuntime",
        ],
    )?;
    reject_unclassified_capability_aggregates(
        &lifecycle_file,
        &[
            "LifecycleWiring",
            "ReportLifecycle",
            "BuildLifecycleRuntime",
            "RecoveryRuntime",
        ],
    )?;
    reject_aggregate_signatures(&lifecycle_file)?;

    let custom_file =
        syn::parse_file(custom).map_err(|error| format!("parse custom host: {error}"))?;
    reject_broad_aliases(&custom_file)?;
    if custom.contains("ReplicatorLifecycleHost")
        || impl_methods(&custom_file, "ReplicatorLifecycleHost").is_some()
        || struct_fields(&custom_file, "ReplicatorLifecycleHost").is_some()
    {
        return Err("universal lifecycle facade remains".into());
    }
    if impl_methods(&custom_file, "ReplicatorLifecycleRegistration").as_ref()
        != Some(&names(&["managed", "service", "from_wiring"]))
    {
        return Err("lifecycle registration assembly methods changed".into());
    }
    let expected_registration_fields = BTreeMap::from([
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
        (
            "build".to_owned(),
            "super::lifecycle::BuildLifecycleRuntime".to_owned(),
        ),
        (
            "build_cancellation".to_owned(),
            "super::lifecycle::BuildCancellationRuntime".to_owned(),
        ),
        (
            "outbound".to_owned(),
            "super::lifecycle::OutboundLifecycleRuntime".to_owned(),
        ),
        (
            "removal_witness".to_owned(),
            "super::lifecycle::RemovalWitnessRuntime".to_owned(),
        ),
        (
            "topology".to_owned(),
            "super::lifecycle::TopologyRuntime".to_owned(),
        ),
        (
            "recovery".to_owned(),
            "super::lifecycle::RecoveryRuntime".to_owned(),
        ),
    ]);
    if struct_fields(&custom_file, "ReplicatorLifecycleRegistration").as_ref()
        != Some(&expected_registration_fields)
    {
        return Err("lifecycle registration retained complete wiring".into());
    }
    reject_unclassified_capability_aggregates(&custom_file, &["ReplicatorLifecycleRegistration"])?;
    reject_aggregate_signatures(&custom_file)?;
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
    if !compact(&queue_cancellation.sig).contains("build:Weak<dynBuildCancellation>") {
        return Err("build queue cancellation no longer receives a weak build owner".into());
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
    for (trait_name, expected_methods) in [
        (
            "BuildHost",
            names(&[
                "is_managed",
                "describe_peer",
                "snapshot",
                "register_peer_session",
                "authorize_build",
                "execute_build",
                "accept_build",
                "prepare_copy",
                "accept_copy_acknowledgement",
                "accept_acknowledgement",
            ]),
        ),
        ("BuildAttemptHost", names(&["generation", "cancel_attempt"])),
        (
            "PeerDiscoveryHost",
            names(&[
                "snapshot",
                "register_peer_session",
                "observe_secondary_removal_witness",
                "accept_acknowledgement",
                "repair_peer",
            ]),
        ),
        ("OutboundHost", names(&["next_outbound", "snapshot"])),
    ] {
        let item = hosting_file
            .items
            .iter()
            .find_map(|item| match item {
                Item::Trait(item) if item.ident == trait_name => Some(item),
                _ => None,
            })
            .ok_or_else(|| format!("{trait_name} missing"))?;
        let methods = item
            .items
            .iter()
            .filter_map(|item| match item {
                TraitItem::Fn(method) => Some(method.sig.ident.to_string()),
                _ => None,
            })
            .collect::<BTreeSet<_>>();
        if compact(&item.supertraits) != "Send+Sync" || methods != expected_methods {
            return Err(format!("{trait_name} capability budget changed"));
        }
    }
    assert_shape(
        &hosting_file,
        "BuildRuntime",
        &[
            ("inner", "Arc<dynBuildHost>"),
            ("cancellation", "BuildAttemptRuntime"),
        ],
        &[
            "describe_peer",
            "generation",
            "cancellation",
            "snapshot",
            "register_peer_session",
            "authorize_build",
            "prepare_copy",
            "accept_copy_acknowledgement",
            "accept_acknowledgement",
            "execute_admitted_build",
        ],
    )?;
    assert_shape(
        &hosting_file,
        "BuildAttemptRuntime",
        &[("inner", "Arc<dynBuildAttemptHost>")],
        &["generation", "cancel_attempt"],
    )?;
    assert_shape(
        &hosting_file,
        "BuildRuntimeCancellation",
        &[
            (
                "decision",
                "Option<tokio::sync::oneshot::Sender<BuildCancellationDecision>>",
            ),
            ("completion", "tokio::task::JoinHandle<Result<()>>"),
        ],
        &["new", "finish"],
    )?;
    assert_shape(
        &hosting_file,
        "PeerDiscoveryRuntime",
        &[("inner", "Arc<dynPeerDiscoveryHost>")],
        &[
            "snapshot",
            "register_peer_session",
            "observe_secondary_removal_witness",
            "accept_acknowledgement",
            "repair_peer",
        ],
    )?;
    assert_shape(
        &hosting_file,
        "OutboundRuntime",
        &[("inner", "Arc<dynOutboundHost>")],
        &["next_outbound", "snapshot"],
    )?;
    reject_capability_escape_paths(
        &hosting_file,
        &[
            "ReportHost",
            "BuildHost",
            "BuildAttemptHost",
            "PeerDiscoveryHost",
            "OutboundHost",
        ],
        &[
            "ReportRuntime",
            "BuildRuntime",
            "BuildAttemptRuntime",
            "PeerDiscoveryRuntime",
            "OutboundRuntime",
        ],
    )?;
    reject_unclassified_capability_aggregates(
        &hosting_file,
        &[
            "RegisteredReplicator",
            "HostedPrimaryReplicator",
            "BuildRuntime",
        ],
    )?;
    reject_aggregate_signatures(&hosting_file)?;
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
            "build_lifecycle".to_owned(),
            "Option<lifecycle::BuildLifecycleRuntime>".to_owned(),
        ),
        (
            "build_cancellation".to_owned(),
            "Option<lifecycle::BuildCancellationRuntime>".to_owned(),
        ),
        (
            "outbound_lifecycle".to_owned(),
            "Option<lifecycle::OutboundLifecycleRuntime>".to_owned(),
        ),
        (
            "removal_witness".to_owned(),
            "Option<lifecycle::RemovalWitnessRuntime>".to_owned(),
        ),
        (
            "topology_lifecycle".to_owned(),
            "Option<lifecycle::TopologyRuntime>".to_owned(),
        ),
        (
            "recovery_lifecycle".to_owned(),
            "Option<lifecycle::RecoveryRuntime>".to_owned(),
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
    if impl_methods(&hosting_file, "RegisteredReplicator").as_ref()
        != Some(&names(&[
            "managed_data_plane",
            "process_lifecycle",
            "authority_lifecycle",
            "peer_lifecycle",
            "access_closure",
            "access_lifecycle",
            "report_lifecycle",
            "lifecycle_evidence",
            "effect_evidence",
            "build_lifecycle",
            "build_cancellation",
            "outbound_lifecycle",
            "removal_witness",
            "topology_lifecycle",
            "recovery_lifecycle",
            "primary",
            "open",
            "change_role",
            "update_epoch",
            "close",
            "current_progress",
            "catch_up_capability",
            "abort",
        ]))
    {
        return Err("registered capability projection methods changed".into());
    }
    let exact_cancellation = impl_method(&hosting_file, "BuildRuntimeCancellation", "new")
        .ok_or_else(|| "BuildRuntimeCancellation::new missing".to_owned())?;
    let cancellation_signature = compact(&exact_cancellation.sig);
    if !cancellation_signature.contains("runtime:BuildAttemptRuntime")
        || cancellation_signature.contains("runtime:BuildRuntime")
    {
        return Err("build runtime cancellation constructor is not cancellation-only".into());
    }
    let cancellation_body = compact(&exact_cancellation.block);
    if cancellation_body.contains("RuntimeHost")
        || cancellation_body.contains("PodRuntime")
        || cancellation_body.contains(".lifecycle")
    {
        return Err("build runtime cancellation captured a broad owner".into());
    }

    let actual_consumers = transition_consumers(hosting)?
        .into_iter()
        .chain(transition_consumers(custom)?)
        .collect::<BTreeSet<_>>();
    let expected_consumers = BTreeSet::new();
    if actual_consumers != expected_consumers {
        return Err(format!(
            "universal lifecycle consumer remains: {actual_consumers:#?}"
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
    let transport = source("src/host/transport.rs");
    validate(&lifecycle, &custom, &hosting).unwrap();

    assert!(hosting.contains("#[path = \"lifecycle.rs\"]\nmod lifecycle;"));
    assert!(
        report.contains("use crate::host::hosting::ReportRuntime;")
            && !report.contains("use crate::host::hosting::PodRuntime;")
            && report.contains("report(&self, runtime: &ReportRuntime)")
    );
    let transport_file = syn::parse_file(&transport).unwrap();
    assert_eq!(
        struct_fields(&transport_file, "GrpcOutboundDispatcher")
            .unwrap()
            .get("runtime")
            .map(String::as_str),
        Some("BuildRuntime")
    );
    let dispatch_cancellation =
        impl_method(&transport_file, "BuildDispatchCancellation", "new").unwrap();
    assert!(compact(&dispatch_cancellation.sig).contains("runtime:BuildAttemptRuntime"));
    let peer_reporter = transport_file
        .items
        .iter()
        .find_map(|item| match item {
            Item::Trait(item) if item.ident == "PeerReporter" => Some(item),
            _ => None,
        })
        .unwrap();
    assert_eq!(
        peer_reporter
            .items
            .iter()
            .filter_map(|item| match item {
                TraitItem::Fn(method) => Some(method.sig.ident.to_string()),
                _ => None,
            })
            .collect::<BTreeSet<_>>(),
        names(&["peer_report"])
    );
    assert!(transport.contains("dispatcher: Arc<dyn PeerReporter>"));
    assert_eq!(
        transport.matches("PodRuntime").count(),
        2,
        "only the cfg(test) cancellation helper may retain PodRuntime"
    );
    let host_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/host");
    for entry in fs::read_dir(&host_root).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().and_then(|extension| extension.to_str()) != Some("rs") {
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

    let broad_facade = format!("{custom}\nstruct ReplicatorLifecycleHost;\n");
    assert!(validate(&lifecycle, &broad_facade, &hosting).is_err());

    let missing_peer_view =
        hosting.replace("    peer_lifecycle: Option<lifecycle::PeerRuntime>,\n", "");
    assert!(validate(&lifecycle, &custom, &missing_peer_view).is_err());

    let broad_alias = format!("{lifecycle}\ntype Everything = LifecycleWiring;\n");
    assert!(validate(&broad_alias, &custom, &hosting).is_err());

    let universal_getter = custom.replace(
        "impl ReplicatorLifecycleRegistration {\n",
        "impl ReplicatorLifecycleRegistration {\n    fn all_capabilities(&self) -> &LifecycleWiring { unreachable!() }\n",
    );
    assert!(validate(&lifecycle, &universal_getter, &hosting).is_err());

    let broad_cancellation = hosting.replace(
        "struct BuildRuntimeCancellation {\n",
        "struct BuildRuntimeCancellation {\n    host: Arc<RuntimeHost>,\n",
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
        "    fn new(runtime: BuildAttemptRuntime, build_id: OperationId, generation: u64) -> Self {\n",
        "    fn new(runtime: BuildAttemptRuntime, build_id: OperationId, generation: u64) -> Self {\n        let broad_host: Option<Arc<RuntimeHost>> = None;\n",
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

    let build_deref = format!(
        "{hosting}\nimpl std::ops::Deref for BuildRuntime {{ type Target = PodRuntime; fn deref(&self) -> &Self::Target {{ unreachable!() }} }}\n"
    );
    assert!(validate(&lifecycle, &custom, &build_deref).is_err());

    let broad_peer_signature = hosting.replace(
        "impl PeerDiscoveryRuntime {\n    pub(crate) async fn snapshot(&self) -> RuntimeSnapshot {\n        self.inner.snapshot().await\n    }\n",
        "impl PeerDiscoveryRuntime {\n    pub(crate) async fn snapshot(&self) -> Arc<RuntimeHost> { unreachable!() }\n",
    );
    assert!(validate(&lifecycle, &custom, &broad_peer_signature).is_err());

    let broad_coordinator_cleanup = hosting.replace(
        "    fn new(runtime: BuildAttemptRuntime, build_id: OperationId, generation: u64) -> Self {\n",
        "    fn new(runtime: BuildRuntime, build_id: OperationId, generation: u64) -> Self {\n",
    );
    assert!(validate(&lifecycle, &custom, &broad_coordinator_cleanup).is_err());

    let concrete_backend_signature = hosting.replace(
        "impl PeerDiscoveryRuntime {\n    pub(crate) async fn snapshot(&self) -> RuntimeSnapshot {\n",
        "impl PeerDiscoveryRuntime {\n    pub(crate) async fn snapshot(&self) -> Arc<custom::CustomReplicatorHost> {\n",
    );
    assert!(validate(&lifecycle, &custom, &concrete_backend_signature).is_err());

    let universal_registration_getter = hosting.replace(
        "impl RegisteredReplicator {\n",
        "impl RegisteredReplicator {\n    fn all_capabilities(&self) -> (&lifecycle::ProcessRuntime, &lifecycle::TopologyRuntime) { unreachable!() }\n",
    );
    assert!(validate(&lifecycle, &custom, &universal_registration_getter).is_err());

    let renamed_wiring = format!(
        "{hosting}\nstruct LifecycleBroker {{ process: lifecycle::ProcessRuntime, authority: lifecycle::AuthorityRuntime, topology: lifecycle::TopologyRuntime }}\n"
    );
    assert!(validate(&lifecycle, &custom, &renamed_wiring).is_err());

    let host_view_broker = format!(
        "{hosting}\nstruct HostViewBroker {{ build: BuildRuntime, report: ReportRuntime, outbound: OutboundRuntime }}\n"
    );
    assert!(validate(&lifecycle, &custom, &host_view_broker).is_err());

    let nested_broker = format!(
        "{hosting}\nmod leaked {{ struct Broker {{ process: lifecycle::ProcessRuntime, topology: lifecycle::TopologyRuntime }} }}\n"
    );
    assert!(validate(&lifecycle, &custom, &nested_broker).is_err());

    let tuple_alias = format!(
        "{hosting}\ntype CapabilityTuple = (BuildRuntime, ReportRuntime, OutboundRuntime);\n"
    );
    assert!(validate(&lifecycle, &custom, &tuple_alias).is_err());

    let tuple_getter = format!(
        "{hosting}\nfn all_views() -> (BuildRuntime, ReportRuntime) {{ unreachable!() }}\n"
    );
    assert!(validate(&lifecycle, &custom, &tuple_getter).is_err());

    let tuple_struct = format!("{hosting}\nstruct TupleBroker((BuildRuntime, ReportRuntime));\n");
    assert!(validate(&lifecycle, &custom, &tuple_struct).is_err());

    let tuple_field =
        format!("{hosting}\nstruct TupleFieldBroker {{ views: (BuildRuntime, ReportRuntime) }}\n");
    assert!(validate(&lifecycle, &custom, &tuple_field).is_err());

    let local_broker = format!(
        "{hosting}\nfn leaked_local() {{ struct LocalBroker {{ build: BuildRuntime, report: ReportRuntime }} }}\n"
    );
    assert!(validate(&lifecycle, &custom, &local_broker).is_err());
}
