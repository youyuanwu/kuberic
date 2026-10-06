use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use quote::ToTokens;
use syn::punctuated::Punctuated;
use syn::visit::Visit;
use syn::{Expr, ImplItem, Item, Lit, Meta, Token, TraitItem, Type};

const CAPABILITY_TRAITS: &[&str] = &[
    "ProcessLifecycle",
    "AuthorityLifecycle",
    "AccessLifecycle",
    "BuildLifecycle",
    "BuildCancellation",
    "TopologyLifecycle",
    "LifecycleObservation",
    "OutboundLifecycle",
];

const CAPABILITY_TYPES: &[&str] = &[
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

const BROAD_OWNER_TYPES: &[&str] = &[
    "RuntimeHost",
    "PodRuntime",
    "RegisteredReplicator",
    "LifecycleWiring",
    "ReplicatorLifecycleRegistration",
    "ManagedLifecycleBackend",
    "CustomReplicatorHost",
];

const LIFECYCLE_VIEW_RULES: &[(&str, &[&str])] = &[
    ("ProcessRuntime", &["dynProcessLifecycle"]),
    ("AuthorityRuntime", &["dynAuthorityLifecycle"]),
    ("PeerRuntime", &["dynAuthorityLifecycle"]),
    ("AccessClosure", &["dynAccessLifecycle"]),
    ("AccessRuntime", &["dynAccessLifecycle"]),
    (
        "ReportLifecycle",
        &["dynAccessLifecycle", "dynLifecycleObservation"],
    ),
    ("EvidenceRuntime", &["dynLifecycleObservation"]),
    ("EffectEvidenceRuntime", &["dynLifecycleObservation"]),
    (
        "BuildLifecycleRuntime",
        &[
            "dynBuildLifecycle",
            "dynBuildCancellation",
            "BuildCancellationRuntime",
        ],
    ),
    ("BuildCancellationRuntime", &["dynBuildCancellation"]),
    ("OutboundLifecycleRuntime", &["dynOutboundLifecycle"]),
    ("RemovalWitnessRuntime", &["dynTopologyLifecycle"]),
    ("TopologyRuntime", &["dynTopologyLifecycle"]),
    (
        "RecoveryRuntime",
        &[
            "dynAuthorityLifecycle",
            "dynAccessLifecycle",
            "dynTopologyLifecycle",
            "dynLifecycleObservation",
        ],
    ),
];

const HOSTING_VIEW_RULES: &[(&str, &[&str])] = &[
    ("ReportRuntime", &[]),
    ("BuildRuntime", &["BuildAttemptRuntime"]),
    ("BuildAttemptRuntime", &[]),
    ("PeerDiscoveryRuntime", &[]),
    ("OutboundRuntime", &[]),
];

fn source(path: &str) -> String {
    fs::read_to_string(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(path)).unwrap()
}

fn compact<T: ToTokens>(value: &T) -> String {
    value.to_token_stream().to_string().replace(' ', "")
}

fn identifiers(value: &str) -> BTreeSet<&str> {
    value
        .split(|character: char| !(character.is_ascii_alphanumeric() || character == '_'))
        .filter(|identifier| !identifier.is_empty())
        .collect()
}

fn matching_markers(value: &str, markers: &[&str]) -> BTreeSet<String> {
    let identifiers = identifiers(value);
    markers
        .iter()
        .filter(|marker| identifiers.contains(**marker))
        .map(|marker| (*marker).to_owned())
        .collect()
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

fn impl_name(item: &syn::ItemImpl) -> String {
    type_name(&item.self_ty).unwrap_or_else(|| "<unknown>".into())
}

fn production_rust_files(root: &Path) -> Vec<PathBuf> {
    fn collect(path: &Path, files: &mut Vec<PathBuf>) {
        for entry in fs::read_dir(path).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                if path.file_name().and_then(|name| name.to_str()) != Some("tests") {
                    collect(&path, files);
                }
            } else if path.extension().and_then(|extension| extension.to_str()) == Some("rs") {
                files.push(path);
            }
        }
    }

    let mut files = Vec::new();
    collect(root, &mut files);
    files.sort();
    files
}

fn lint_exempt_module(relative: &Path) -> bool {
    matches!(
        relative.to_str(),
        Some(
            "hosting.rs"
                | "lifecycle.rs"
                | "custom.rs"
                | "custom_removal.rs"
                | "process.rs"
                | "runtime_adapter.rs"
                | "service.rs"
                | "testing.rs"
        )
    )
}

fn allowed_aggregates(relative: &Path) -> &'static [&'static str] {
    match relative.to_str() {
        Some("lifecycle.rs") => &[
            "LifecycleWiring",
            "ReportLifecycle",
            "BuildLifecycleRuntime",
            "RecoveryRuntime",
        ],
        Some("custom.rs") => &[
            "ReplicatorLifecycleRegistration",
            "AcceptedAccessEffect",
            "ManagedLifecycleBackend",
            "CustomReplicatorHost",
        ],
        Some("hosting.rs") => &[
            "RegisteredReplicator",
            "HostedPrimaryReplicator",
            "BuildRuntime",
            "PodRuntime",
            "RuntimeDataPlane",
            "RuntimeHost",
            "HostAccessView",
            "OpenAttempt",
        ],
        Some("process.rs") => &["ReplicaHandle"],
        Some("service.rs") => &["AgentService"],
        Some("testing.rs") => &["Endpoint"],
        _ => &[],
    }
}

fn reject_guarded_aliases(file: &syn::File, reject_broad: bool) -> Result<(), String> {
    struct AliasVisitor {
        reject_broad: bool,
        issue: Option<String>,
    }

    fn inspect_use(tree: &syn::UseTree, reject_broad: bool, issue: &mut Option<String>) {
        match tree {
            syn::UseTree::Path(path) => inspect_use(&path.tree, reject_broad, issue),
            syn::UseTree::Rename(rename) => {
                let target = rename.ident.to_string();
                if CAPABILITY_TYPES.contains(&target.as_str())
                    || (reject_broad && BROAD_OWNER_TYPES.contains(&target.as_str()))
                {
                    *issue = Some(format!(
                        "guarded import alias {} -> {target}",
                        rename.rename
                    ));
                }
            }
            syn::UseTree::Group(group) => {
                for tree in &group.items {
                    inspect_use(tree, reject_broad, issue);
                }
            }
            _ => {}
        }
    }

    impl<'ast> Visit<'ast> for AliasVisitor {
        fn visit_item_use(&mut self, item: &'ast syn::ItemUse) {
            inspect_use(&item.tree, self.reject_broad, &mut self.issue);
            syn::visit::visit_item_use(self, item);
        }

        fn visit_item_type(&mut self, item: &'ast syn::ItemType) {
            let target = compact(&item.ty);
            if !matching_markers(&target, CAPABILITY_TYPES).is_empty()
                || (self.reject_broad && !matching_markers(&target, BROAD_OWNER_TYPES).is_empty())
            {
                self.issue = Some(format!("guarded type alias {} -> {target}", item.ident));
            }
            syn::visit::visit_item_type(self, item);
        }
    }

    let mut visitor = AliasVisitor {
        reject_broad,
        issue: None,
    };
    visitor.visit_file(file);
    match visitor.issue {
        Some(issue) => Err(issue),
        None => Ok(()),
    }
}

fn reject_unapproved_aggregates(
    file: &syn::File,
    allowed: &[&str],
    reject_broad: bool,
) -> Result<(), String> {
    struct AggregateVisitor<'a> {
        allowed: &'a [&'a str],
        reject_broad: bool,
        depth: usize,
        issue: Option<String>,
    }

    impl<'ast> Visit<'ast> for AggregateVisitor<'_> {
        fn visit_item_struct(&mut self, item: &'ast syn::ItemStruct) {
            let capabilities = item
                .fields
                .iter()
                .flat_map(|field| matching_markers(&compact(&field.ty), CAPABILITY_TYPES))
                .collect::<BTreeSet<_>>();
            let broad = item
                .fields
                .iter()
                .flat_map(|field| matching_markers(&compact(&field.ty), BROAD_OWNER_TYPES))
                .collect::<BTreeSet<_>>();
            let approved =
                self.depth == 0 && self.allowed.contains(&item.ident.to_string().as_str());
            if capabilities.len() >= 2 && !approved {
                self.issue = Some(format!(
                    "{} is an unapproved capability aggregate",
                    item.ident
                ));
            } else if self.reject_broad && !broad.is_empty() && !approved {
                self.issue = Some(format!("{} retains an unapproved broad owner", item.ident));
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
        reject_broad,
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
    struct SignatureVisitor {
        issue: Option<String>,
    }

    fn inspect(owner: &str, signature: &syn::Signature) -> Option<String> {
        let signature = compact(signature);
        let capabilities = matching_markers(&signature, CAPABILITY_TYPES);
        let broad = matching_markers(&signature, BROAD_OWNER_TYPES);
        (capabilities.len() + broad.len() >= 2)
            .then(|| format!("{owner} exposes an aggregate capability signature"))
    }

    impl<'ast> Visit<'ast> for SignatureVisitor {
        fn visit_item_fn(&mut self, item: &'ast syn::ItemFn) {
            self.issue = self
                .issue
                .take()
                .or_else(|| inspect(&format!("fn {}", item.sig.ident), &item.sig));
            syn::visit::visit_item_fn(self, item);
        }

        fn visit_item_trait(&mut self, item: &'ast syn::ItemTrait) {
            for member in &item.items {
                if let TraitItem::Fn(method) = member {
                    self.issue = self.issue.take().or_else(|| {
                        inspect(
                            &format!("{}::{}", item.ident, method.sig.ident),
                            &method.sig,
                        )
                    });
                }
            }
            syn::visit::visit_item_trait(self, item);
        }

        fn visit_item_impl(&mut self, item: &'ast syn::ItemImpl) {
            for member in &item.items {
                if let ImplItem::Fn(method) = member {
                    self.issue = self.issue.take().or_else(|| {
                        inspect(
                            &format!("{}::{}", impl_name(item), method.sig.ident),
                            &method.sig,
                        )
                    });
                }
            }
            syn::visit::visit_item_impl(self, item);
        }
    }

    let mut visitor = SignatureVisitor { issue: None };
    visitor.visit_file(file);
    match visitor.issue {
        Some(issue) => Err(issue),
        None => Ok(()),
    }
}

fn validate_capability_traits(file: &syn::File) -> Result<(), String> {
    let mut found = BTreeSet::new();
    for item in &file.items {
        let Item::Trait(item) = item else {
            continue;
        };
        let capability_supertraits =
            matching_markers(&compact(&item.supertraits), CAPABILITY_TRAITS);
        if capability_supertraits.len() >= 2 {
            return Err(format!(
                "{} combines multiple lifecycle capability traits",
                item.ident
            ));
        }
        if !CAPABILITY_TRAITS.contains(&item.ident.to_string().as_str()) {
            continue;
        }
        found.insert(item.ident.to_string());
        for member in &item.items {
            let TraitItem::Fn(method) = member else {
                continue;
            };
            let signature = compact(&method.sig);
            if !matching_markers(&signature, CAPABILITY_TYPES).is_empty()
                || !matching_markers(&signature, CAPABILITY_TRAITS).is_empty()
                || !matching_markers(&signature, BROAD_OWNER_TYPES).is_empty()
            {
                return Err(format!(
                    "{}::{} exposes another lifecycle capability",
                    item.ident, method.sig.ident
                ));
            }
        }
    }
    let expected = CAPABILITY_TRAITS
        .iter()
        .map(|name| (*name).to_owned())
        .collect::<BTreeSet<_>>();
    if found != expected {
        return Err(format!(
            "lifecycle capability trait set changed: {found:#?}"
        ));
    }
    Ok(())
}

fn validate_view_boundaries(file: &syn::File, rules: &[(&str, &[&str])]) -> Result<(), String> {
    for (view, allowed) in rules {
        let item = file
            .items
            .iter()
            .find_map(|item| match item {
                Item::Struct(item) if item.ident == *view => Some(item),
                _ => None,
            })
            .ok_or_else(|| format!("{view} missing"))?;
        for field in &item.fields {
            let field_type = compact(&field.ty);
            if !matching_markers(&field_type, BROAD_OWNER_TYPES).is_empty() {
                return Err(format!("{view} retains a broad owner"));
            }
            let capabilities = matching_markers(&field_type, CAPABILITY_TYPES);
            if capabilities
                .iter()
                .any(|capability| !allowed.contains(&capability.as_str()))
            {
                return Err(format!("{view} retains an unrelated capability"));
            }
        }
        for item in &file.items {
            let Item::Impl(item) = item else {
                continue;
            };
            if type_name(&item.self_ty).as_deref() != Some(*view) {
                continue;
            }
            if item.trait_.is_some() {
                return Err(format!("{view} implements an unapproved conversion trait"));
            }
            for member in &item.items {
                let ImplItem::Fn(method) = member else {
                    continue;
                };
                let signature = compact(&method.sig);
                if !matching_markers(&signature, BROAD_OWNER_TYPES).is_empty() {
                    return Err(format!(
                        "{view}::{} exposes a broad owner",
                        method.sig.ident
                    ));
                }
                let capabilities = matching_markers(&signature, CAPABILITY_TYPES);
                if capabilities
                    .iter()
                    .any(|capability| !allowed.contains(&capability.as_str()))
                {
                    return Err(format!(
                        "{view}::{} exposes an unrelated capability",
                        method.sig.ident
                    ));
                }
            }
        }
    }
    Ok(())
}

fn cfg_enabled_in_production(meta: &Meta) -> bool {
    match meta {
        Meta::Path(path) if path.is_ident("test") || path.is_ident("kuberic_workspace_tests") => {
            false
        }
        Meta::Path(_) => true,
        Meta::NameValue(value) if value.path.is_ident("feature") => !matches!(
            &value.value,
            Expr::Lit(value) if matches!(&value.lit, Lit::Str(value) if value.value() == "testing")
        ),
        Meta::NameValue(_) => true,
        Meta::List(list) => {
            let arguments = list
                .parse_args_with(Punctuated::<Meta, Token![,]>::parse_terminated)
                .unwrap_or_default();
            if list.path.is_ident("all") {
                arguments.iter().all(cfg_enabled_in_production)
            } else if list.path.is_ident("any") {
                arguments.iter().any(cfg_enabled_in_production)
            } else if list.path.is_ident("not") && arguments.len() == 1 {
                !cfg_enabled_in_production(arguments.first().unwrap())
            } else {
                true
            }
        }
    }
}

fn excluded_from_production(attributes: &[syn::Attribute]) -> bool {
    attributes
        .iter()
        .filter(|attribute| attribute.path().is_ident("cfg"))
        .filter_map(|attribute| attribute.parse_args::<Meta>().ok())
        .any(|meta| !cfg_enabled_in_production(&meta))
}

fn validate_fixture_gates(file: &syn::File) -> Result<(), String> {
    for (owner, methods, is_trait) in [
        ("ProcessLifecycle", &["invalidate_public_access"][..], true),
        (
            "BuildLifecycle",
            &["build_replica", "remove_replica"][..],
            true,
        ),
        ("ProcessRuntime", &["invalidate_public_access"][..], false),
        (
            "BuildLifecycleRuntime",
            &["build_replica", "remove_replica"][..],
            false,
        ),
    ] {
        for method_name in methods {
            let attributes = if is_trait {
                file.items.iter().find_map(|item| match item {
                    Item::Trait(item) if item.ident == owner => {
                        item.items.iter().find_map(|member| match member {
                            TraitItem::Fn(method) if method.sig.ident == *method_name => {
                                Some(method.attrs.as_slice())
                            }
                            _ => None,
                        })
                    }
                    _ => None,
                })
            } else {
                file.items.iter().find_map(|item| match item {
                    Item::Impl(item) if type_name(&item.self_ty).as_deref() == Some(owner) => {
                        item.items.iter().find_map(|member| match member {
                            ImplItem::Fn(method) if method.sig.ident == *method_name => {
                                Some(method.attrs.as_slice())
                            }
                            _ => None,
                        })
                    }
                    _ => None,
                })
            }
            .ok_or_else(|| format!("{owner}::{method_name} missing"))?;
            if !excluded_from_production(attributes) {
                return Err(format!(
                    "{owner}::{method_name} is not excluded from production"
                ));
            }
        }
    }
    Ok(())
}

fn reject_retained_wiring(file: &syn::File, owner: &str, forbidden: &[&str]) -> Result<(), String> {
    let item = file
        .items
        .iter()
        .find_map(|item| match item {
            Item::Struct(item) if item.ident == owner => Some(item),
            _ => None,
        })
        .ok_or_else(|| format!("{owner} missing"))?;
    for field in &item.fields {
        let field_type = compact(&field.ty);
        if forbidden
            .iter()
            .any(|forbidden| identifiers(&field_type).contains(forbidden))
        {
            return Err(format!("{owner} retains complete lifecycle wiring"));
        }
    }
    Ok(())
}

fn validate_transport_routing(source: &str) -> Result<(), String> {
    struct CallVisitor {
        admitted: usize,
        direct: usize,
    }

    impl<'ast> Visit<'ast> for CallVisitor {
        fn visit_expr_method_call(&mut self, call: &'ast syn::ExprMethodCall) {
            match call.method.to_string().as_str() {
                "execute_admitted_build" => self.admitted += 1,
                "build_replica" => self.direct += 1,
                _ => {}
            }
            syn::visit::visit_expr_method_call(self, call);
        }
    }

    let file = syn::parse_file(source).map_err(|error| format!("parse transport: {error}"))?;
    let mut visitor = CallVisitor {
        admitted: 0,
        direct: 0,
    };
    visitor.visit_file(&file);
    if visitor.admitted != 1 {
        return Err(format!(
            "transport has {} admitted-build coordinator call sites",
            visitor.admitted
        ));
    }
    if visitor.direct != 0 {
        return Err("transport contains a direct primary build call".into());
    }
    Ok(())
}

fn validate_production_module(relative: &Path, source: &str) -> Result<(), String> {
    let display = relative.to_string_lossy();
    for forbidden in ["ReplicatorLifecycleBackend", "ReplicatorLifecycleHost"] {
        if source.contains(forbidden) {
            return Err(format!("{display}: universal lifecycle facade {forbidden}"));
        }
    }
    let file = syn::parse_file(source).map_err(|error| format!("{display}: {error}"))?;
    let reject_broad = lint_exempt_module(relative);
    reject_guarded_aliases(&file, reject_broad).map_err(|error| format!("{display}: {error}"))?;
    reject_unapproved_aggregates(&file, allowed_aggregates(relative), reject_broad)
        .map_err(|error| format!("{display}: {error}"))?;
    reject_aggregate_signatures(&file).map_err(|error| format!("{display}: {error}"))?;
    Ok(())
}

fn validate_project() -> Result<(), String> {
    let host_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/host");
    for path in production_rust_files(&host_root) {
        let relative = path.strip_prefix(&host_root).unwrap();
        let source = fs::read_to_string(&path).unwrap();
        validate_production_module(relative, &source)?;
    }

    let lifecycle = source("src/host/lifecycle.rs");
    let lifecycle_file =
        syn::parse_file(&lifecycle).map_err(|error| format!("parse lifecycle: {error}"))?;
    validate_capability_traits(&lifecycle_file)?;
    validate_view_boundaries(&lifecycle_file, LIFECYCLE_VIEW_RULES)?;
    validate_fixture_gates(&lifecycle_file)?;

    let custom = source("src/host/custom.rs");
    let custom_file = syn::parse_file(&custom).map_err(|error| format!("parse custom: {error}"))?;
    reject_retained_wiring(
        &custom_file,
        "ReplicatorLifecycleRegistration",
        &[
            "LifecycleWiring",
            "ManagedLifecycleBackend",
            "CustomReplicatorHost",
        ],
    )?;

    let hosting = source("src/host/hosting.rs");
    let hosting_file =
        syn::parse_file(&hosting).map_err(|error| format!("parse hosting: {error}"))?;
    validate_view_boundaries(&hosting_file, HOSTING_VIEW_RULES)?;
    reject_retained_wiring(
        &hosting_file,
        "RegisteredReplicator",
        &[
            "LifecycleWiring",
            "ReplicatorLifecycleRegistration",
            "ManagedLifecycleBackend",
            "CustomReplicatorHost",
        ],
    )?;

    validate_transport_routing(&source("src/host/transport.rs"))?;
    Ok(())
}

fn assert_rejected(result: Result<(), String>, expected: &str) {
    let error = result.expect_err("mutation unexpectedly passed validation");
    assert!(
        error.contains(expected),
        "diagnostic did not identify {expected:?}: {error}"
    );
}

#[test]
fn lifecycle_capability_boundaries_are_narrow() {
    validate_project().unwrap();
}

#[test]
fn lifecycle_capability_guard_rejects_representative_escapes() {
    let aggregate_trait = syn::parse_file(
        "trait ProcessLifecycle {}\ntrait BuildLifecycle {}\ntrait UniversalLifecycle: ProcessLifecycle + BuildLifecycle {}",
    )
    .unwrap();
    assert_rejected(
        validate_capability_traits(&aggregate_trait),
        "UniversalLifecycle",
    );

    let lifecycle = source("src/host/lifecycle.rs");
    let trait_escape = lifecycle.replace(
        "    async fn settle_primary_prefix(&self) -> Result<()>;\n",
        "    async fn settle_primary_prefix(&self) -> Result<()>;\n    fn build(&self) -> Arc<dyn BuildLifecycle>;\n",
    );
    assert_rejected(
        validate_capability_traits(&syn::parse_file(&trait_escape).unwrap()),
        "ProcessLifecycle::build",
    );

    let harmless_method = format!(
        "{lifecycle}\nimpl ProcessRuntime {{ fn harmless_narrow_helper(&self) -> bool {{ true }} }}\n"
    );
    validate_view_boundaries(
        &syn::parse_file(&harmless_method).unwrap(),
        LIFECYCLE_VIEW_RULES,
    )
    .unwrap();

    let alias = syn::parse_file(
        "use self::BuildRuntime as B;\ntype R = ReportRuntime;\nstruct Broker { build: B, report: R }",
    )
    .unwrap();
    assert_rejected(reject_guarded_aliases(&alias, false), "guarded");

    let aggregate = syn::parse_file(
        "struct Broker { build: BuildRuntime, report: ReportRuntime }\nfn local() { struct Local(BuildRuntime, ReportRuntime); }",
    )
    .unwrap();
    assert_rejected(
        reject_unapproved_aggregates(&aggregate, &[], false),
        "aggregate",
    );

    let broad = syn::parse_file("struct UniversalFacade { host: Arc<RuntimeHost> }").unwrap();
    assert_rejected(
        reject_unapproved_aggregates(&broad, &[], true),
        "UniversalFacade",
    );

    let signature =
        syn::parse_file("fn all_views() -> (BuildRuntime, ReportRuntime) { unreachable!() }")
            .unwrap();
    assert_rejected(reject_aggregate_signatures(&signature), "all_views");

    let view_escape = format!(
        "{lifecycle}\nimpl ProcessRuntime {{ fn build_capability(&self) -> BuildLifecycleRuntime {{ unreachable!() }} }}\n"
    );
    assert_rejected(
        validate_view_boundaries(
            &syn::parse_file(&view_escape).unwrap(),
            LIFECYCLE_VIEW_RULES,
        ),
        "build_capability",
    );

    let conversion = format!(
        "{lifecycle}\nimpl std::ops::Deref for ProcessRuntime {{ type Target = RuntimeHost; fn deref(&self) -> &Self::Target {{ unreachable!() }} }}\n"
    );
    assert_rejected(
        validate_view_boundaries(&syn::parse_file(&conversion).unwrap(), LIFECYCLE_VIEW_RULES),
        "conversion",
    );

    let ungated_fixture = lifecycle.replacen(
        "#[cfg(any(all(test, kuberic_workspace_tests), feature = \"testing\"))]",
        "#[cfg(all())]",
        1,
    );
    assert_rejected(
        validate_fixture_gates(&syn::parse_file(&ungated_fixture).unwrap()),
        "production",
    );

    assert_rejected(
        validate_transport_routing(
            "fn dispatch(runtime: BuildRuntime) { runtime.execute_admitted_build(); runtime.execute_admitted_build(); }",
        ),
        "2 admitted-build",
    );
    assert_rejected(
        validate_transport_routing(
            "fn dispatch(runtime: BuildRuntime, primary: PrimaryReplicator) { runtime.execute_admitted_build(); primary.build_replica(); }",
        ),
        "direct primary",
    );

    let module_root = tempfile::tempdir().unwrap();
    let workers = module_root.path().join("workers");
    fs::create_dir(&workers).unwrap();
    fs::write(
        workers.join("hosting.rs"),
        "struct NestedBroker { build: BuildRuntime, report: ReportRuntime }",
    )
    .unwrap();
    let path = production_rust_files(module_root.path())
        .into_iter()
        .next()
        .unwrap();
    let relative = path.strip_prefix(module_root.path()).unwrap();
    let source = fs::read_to_string(&path).unwrap();
    assert_rejected(
        validate_production_module(relative, &source),
        "workers/hosting.rs",
    );
}
