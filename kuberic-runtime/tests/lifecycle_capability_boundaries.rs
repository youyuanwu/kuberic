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
                TraitItem::Fn(method) => Some(method.sig.ident.to_string()),
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

    let custom_file =
        syn::parse_file(custom).map_err(|error| format!("parse custom host: {error}"))?;
    let expected_facade = names(&[
        "managed",
        "service",
        "process_runtime",
        "authority_runtime",
        "peer_runtime",
        "access_closure",
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

    let hosting_file =
        syn::parse_file(hosting).map_err(|error| format!("parse hosting: {error}"))?;
    let mut registered_fields = BTreeMap::new();
    for item in &hosting_file.items {
        let Item::Struct(item) = item else {
            continue;
        };
        if item.ident != "RegisteredReplicator" {
            continue;
        }
        for field in &item.fields {
            let Some(ident) = &field.ident else {
                continue;
            };
            registered_fields.insert(
                ident.to_string(),
                field.ty.to_token_stream().to_string().replace(' ', ""),
            );
        }
    }
    for (field, expected_type) in [
        ("process_lifecycle", "Option<lifecycle::ProcessRuntime>"),
        ("authority_lifecycle", "Option<lifecycle::AuthorityRuntime>"),
        ("peer_lifecycle", "Option<lifecycle::PeerRuntime>"),
        ("access_closure", "Option<lifecycle::AccessClosure>"),
    ] {
        if registered_fields.get(field).map(String::as_str) != Some(expected_type) {
            return Err(format!(
                "registered lifecycle field {field} missing or changed: {registered_fields:#?}"
            ));
        }
    }

    Ok(())
}

#[test]
fn lifecycle_capabilities_have_bounded_method_and_consumer_budgets() {
    validate(
        &source("src/host/lifecycle.rs"),
        &source("src/host/custom.rs"),
        &source("src/host/hosting.rs"),
    )
    .unwrap();
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
}
