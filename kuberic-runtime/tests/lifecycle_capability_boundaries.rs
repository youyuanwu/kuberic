use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use syn::{Item, TraitItem};

fn manifest() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn source(path: &str) -> String {
    std::fs::read_to_string(manifest().join(path)).unwrap()
}

fn trait_methods(path: &str, trait_name: &str) -> BTreeSet<String> {
    let file = syn::parse_file(&source(path)).unwrap();
    file.items
        .into_iter()
        .find_map(|item| match item {
            Item::Trait(item) if item.ident == trait_name => Some(
                item.items
                    .into_iter()
                    .filter_map(|item| match item {
                        TraitItem::Fn(method) => Some(method.sig.ident.to_string()),
                        _ => None,
                    })
                    .collect(),
            ),
            _ => None,
        })
        .unwrap_or_else(|| panic!("missing trait {trait_name} in {path}"))
}

#[test]
fn protected_public_trait_method_sets_are_unchanged() {
    assert_eq!(
        trait_methods("src/replicator/traits.rs", "Replicator"),
        [
            "abort",
            "catch_up_capability",
            "change_role",
            "close",
            "current_progress",
            "open",
            "update_epoch",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect()
    );
    assert_eq!(
        trait_methods("src/replicator/traits.rs", "PrimaryReplicator"),
        [
            "build_replica",
            "on_data_loss",
            "remove_replica",
            "update_catch_up_replica_set_configuration",
            "update_current_replica_set_configuration",
            "wait_for_catch_up_quorum",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect()
    );
    assert_eq!(
        trait_methods("src/replicator/traits.rs", "StateReplicator"),
        [
            "begin_replicate",
            "copy_operations",
            "replicate",
            "replication_operations",
            "update_replicator_settings",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect()
    );
    assert_eq!(
        trait_methods("src/traits.rs", "StatefulServiceReplica"),
        ["abort", "change_role", "close", "open"]
            .into_iter()
            .map(str::to_owned)
            .collect()
    );
    assert_eq!(
        trait_methods("src/traits.rs", "StateProvider"),
        [
            "get_copy_context",
            "get_copy_state",
            "last_committed_lsn",
            "on_data_loss",
            "update_epoch",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect()
    );
    assert_eq!(
        trait_methods("src/replicator/traits.rs", "ReplicatorFactory"),
        ["create_replicator"]
            .into_iter()
            .map(str::to_owned)
            .collect()
    );
    assert_eq!(
        trait_methods("src/engine.rs", "DurableState"),
        [
            "apply",
            "apply_copy_chunk",
            "commit",
            "durable_progress",
            "finish_copy",
            "get_replication_operations",
            "verify_applied",
            "verify_copy_chunk",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect()
    );
}

#[test]
fn replacement_runtime_has_no_managed_default_or_agent_replication_owner() {
    assert!(!Path::new(&manifest().join("src/runtime.rs")).exists());
    assert!(!manifest().join("src/replicator/stateless/mod.rs").exists());
    let replicator = format!(
        "{}\n{}",
        source("src/replicator/mod.rs"),
        source("src/replicator/traits.rs")
    );
    for forbidden in [
        "ManagedReplicatorLifecycle",
        "ManagedReplicatorDataPlane",
        "ManagedReplicaStore",
        "DefaultReplicatorInner",
        "LegacyManaged",
        "StatelessReplicator",
        "StatelessRuntimeControl",
    ] {
        assert!(!replicator.contains(forbidden), "{forbidden} survived");
    }
    let service = source("src/host/service.rs");
    assert!(!service.contains("impl<S, E> proto::replication_data_server::ReplicationData"));
    let transport = source("src/host/transport.rs");
    assert!(!transport.contains("GrpcOutboundDispatcher"));
    assert!(!transport.contains("ReliableTransport"));
}

#[test]
fn native_production_owners_remain_private() {
    let replicator = syn::parse_file(&source("src/replicator/mod.rs")).unwrap();
    let native = replicator
        .items
        .iter()
        .find_map(|item| match item {
            Item::Mod(item) if item.ident == "v1" => Some(item),
            _ => None,
        })
        .expect("native owner module");
    assert!(!matches!(native.vis, syn::Visibility::Public(_)));
    let creation = replicator
        .items
        .iter()
        .find_map(|item| match item {
            Item::Struct(item) if item.ident == "ReplicatorCreation" => Some(item),
            _ => None,
        })
        .expect("private creation owner");
    assert!(!matches!(creation.vis, syn::Visibility::Public(_)));
    assert!(!source("src/replicator/mod.rs").contains("NativeRuntimeControl"));

    let adapter = syn::parse_file(&source("src/replicator/v1/service_adapter.rs")).unwrap();
    for owner in ["production", "production_transport"] {
        let module = adapter
            .items
            .iter()
            .find_map(|item| match item {
                Item::Mod(item) if item.ident == owner => Some(item),
                _ => None,
            })
            .unwrap_or_else(|| panic!("missing native owner {owner}"));
        assert!(!matches!(module.vis, syn::Visibility::Public(_)));
    }
}

#[test]
fn public_operation_owners_are_ordinary_modules() {
    let host = source("src/host/mod.rs");
    assert!(host.contains("mod operation;"));
    assert!(host.contains("mod operation_recovery;"));
    assert!(host.contains("pub(crate) mod public_lifecycle;"));
    assert!(!host.contains("feature = \"testing\""));
}
