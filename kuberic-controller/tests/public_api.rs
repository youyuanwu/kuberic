use std::process::Command;

use kube::CustomResourceExt;
use kuberic_controller::control::{WireError, proto};
use kuberic_controller::crd::{KubericSet, KubericSetStatus};
use kuberic_controller::default_main;
use kuberic_controller::protocol::PROTOCOL_VERSION;

#[test]
fn controller_exports_default_main() {
    fn assert_future<F>(_: F)
    where
        F: std::future::Future<Output = Result<(), Box<dyn std::error::Error>>>,
    {
    }

    assert_future(default_main());
}

#[test]
fn controller_control_exports_preserve_protocol_fencing() {
    let unsupported = PROTOCOL_VERSION + 1;
    let expected = WireError::UnsupportedProtocolVersion {
        expected: PROTOCOL_VERSION,
        observed: unsupported,
    };
    assert_eq!(
        kuberic_controller::control::normalize_public_v1_agent_status_report(
            proto::AgentStatusReport {
                protocol_version: unsupported,
                ..Default::default()
            }
        ),
        Err(expected.clone())
    );
    assert_eq!(
        kuberic_controller::control::validate_execute_request(&proto::ExecuteCommandRequest {
            protocol_version: unsupported,
            ..Default::default()
        }),
        Err(expected)
    );
    assert!(kuberic_controller::control::ensure_supported_version(PROTOCOL_VERSION).is_ok());
}

#[test]
fn crdgen_preserves_public_v1_only_typed_status() {
    let crdgen = std::env::var_os("CARGO_BIN_EXE_crdgen")
        .expect("test runner did not provide the crdgen binary");
    let output = Command::new(crdgen).output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let generated: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let checked_in: serde_json::Value =
        serde_json::from_str(include_str!("../deploy/crd.json")).unwrap();
    assert_eq!(generated, checked_in);
    assert_eq!(generated, serde_json::to_value(KubericSet::crd()).unwrap());

    let status = serde_json::to_value(KubericSetStatus::default()).unwrap();
    assert_eq!(status, serde_json::json!({}));
    assert!(
        serde_json::from_value::<KubericSetStatus>(serde_json::json!({
            "initialized": false,
            "observedGeneration": 0
        }))
        .is_err()
    );
}

#[test]
fn shipped_sample_matches_ordinary_hosting_format() {
    use kuberic_controller::crd::PublicV1OperationStatus;
    use kuberic_runtime::protocol::public_operations::{
        ORDINARY_ACTION_FORMAT, PUBLIC_V1_PROTOCOL_VERSION,
    };
    use kuberic_runtime::protocol::types::{
        AgentGeneration, OperationId, ReplicaId, ReplicaIdentity, ReplicaInstanceId,
    };

    let sample: KubericSet =
        serde_yaml_ng::from_str(include_str!("../../examples/kvstore2/deploy/sample.yaml"))
            .unwrap();
    assert_eq!(ORDINARY_ACTION_FORMAT, 16);
    assert_eq!(sample.spec.hosting_format, ORDINARY_ACTION_FORMAT);
    assert_eq!(sample.spec.hosting_format, PUBLIC_V1_PROTOCOL_VERSION);
    let schema = serde_json::to_value(KubericSet::crd()).unwrap();
    let format = &schema["spec"]["versions"][0]["schema"]["openAPIV3Schema"]["properties"]["spec"]
        ["properties"]["hostingFormat"];
    assert_eq!(format["minimum"].as_f64(), Some(16.0));
    assert_eq!(format["maximum"].as_f64(), Some(16.0));
    sample.validate_ordinary_hosting().unwrap();

    let mut old = sample.clone();
    old.spec.hosting_format = 15;
    let before = old.clone();
    assert!(old.validate_ordinary_hosting().is_err());
    assert_eq!(old, before);
    let mut mixed = sample;
    mixed.status = Some(KubericSetStatus {
        public_v1_operation: Some(PublicV1OperationStatus {
            target: ReplicaIdentity {
                replica_id: ReplicaId::new(1),
                instance_id: ReplicaInstanceId::new("predecessor"),
                agent_generation: AgentGeneration::new("predecessor"),
            },
            operation_id: OperationId::new("retained-callback-program"),
            revision: 1,
            canonical_command_json: "{}".into(),
        }),
        ..Default::default()
    });
    let before = mixed.clone();
    assert!(
        mixed
            .validate_ordinary_hosting()
            .unwrap_err()
            .contains("legacy retained command")
    );
    assert_eq!(mixed, before);
}

#[test]
fn deployment_sources_installation_namespace_from_metadata_namespace_only() {
    let deployment: serde_yaml_ng::Value =
        serde_yaml_ng::from_str(include_str!("../deploy/deployment.yaml")).unwrap();
    let containers = deployment["spec"]["template"]["spec"]["containers"]
        .as_sequence()
        .unwrap();
    let env = containers[0]["env"].as_sequence().unwrap();
    let installation = env
        .iter()
        .find(|entry| entry["name"].as_str() == Some("KUBERIC_INSTALLATION_NAMESPACE"))
        .expect("controller deployment lacks KUBERIC_INSTALLATION_NAMESPACE");
    assert_eq!(
        installation["valueFrom"]["fieldRef"]["fieldPath"].as_str(),
        Some("metadata.namespace")
    );
    assert!(
        env.iter()
            .all(|entry| entry["name"].as_str() != Some("KUBERIC_NAMESPACE"))
    );
}

#[test]
fn deployment_prevents_concurrent_controller_reconciliation() {
    let deployment: serde_yaml_ng::Value =
        serde_yaml_ng::from_str(include_str!("../deploy/deployment.yaml")).unwrap();
    assert_eq!(deployment["spec"]["replicas"].as_i64(), Some(1));
    assert_eq!(
        deployment["spec"]["strategy"]["type"].as_str(),
        Some("Recreate")
    );
}

#[test]
fn production_inventory_contains_only_public_v1_controller_surfaces() {
    let manifest = include_str!("../Cargo.toml");
    let protocol = include_str!("../../kuberic-runtime/src/protocol/command.rs");
    let protobuf = include_str!("../../kuberic-runtime/proto/kuberic.proto");
    let controller = include_str!("../src/lib.rs");

    assert!(!manifest.contains("default = [\"public-v1\"]"));
    assert!(!manifest.contains("public-v1 = [\"kuberic-runtime/testing\"]"));
    for legacy in [
        "PrepareSwitchover(Box<",
        "EnsureConfiguration(Box<",
        "EnsureReplicaBuild(Box<",
        "PrepareSecondaryRemoval(Box<",
        "RetireReplica(Box<",
        "AcceptSecondaryRemovalCommit(Box<",
        "RestartReplicaProcess(Box<",
        "DropReplicaIncarnation(Box<",
    ] {
        assert!(!protocol.contains(legacy), "{legacy}");
    }
    for legacy in [
        "ensure_configuration = 11",
        "ensure_replica_build = 12",
        "prepare_switchover = 13",
        "prepare_secondary_removal = 14",
        "retire_replica = 15",
        "accept_secondary_removal_commit = 16",
    ] {
        assert!(!protobuf.contains(legacy), "{legacy}");
    }
    assert!(!controller.contains("pub mod normalize"));
    assert!(!controller.contains("pub mod plan"));
    assert!(!controller.contains("pub use evaluator::{EvaluationConfig, evaluate}"));
}
