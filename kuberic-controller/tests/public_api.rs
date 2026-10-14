use std::process::Command;

use kube::CustomResourceExt;
use kuberic_controller::control::{WireError, proto};
use kuberic_controller::crd::{KubericSet, KubericSetStatus};
use kuberic_controller::default_main;
use kuberic_controller::protocol::PROTOCOL_VERSION;
use kuberic_controller::protocol::types::AcceptedStatus;
use serde_json::json;

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
        kuberic_controller::control::validate_agent_status_report(&proto::AgentStatusReport {
            protocol_version: unsupported,
            ..Default::default()
        }),
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
fn crdgen_preserves_schema_and_flattened_status() {
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

    let serialized = json!({
        "initialized": false,
        "observedGeneration": 0,
        "conditions": []
    });
    let status: KubericSetStatus = serde_json::from_value(serialized).unwrap();
    assert_eq!(status.authority, AcceptedStatus::default());
    assert_eq!(
        serde_json::to_value(status).unwrap(),
        json!({
            "initialized": false,
            "observedGeneration": 0,
            "effectivePolicy": null,
            "topology": null,
            "transition": null,
            "provisioning": null,
            "primaryFailure": null,
            "quorumLoss": null,
            "lastSwitchover": null,
            "conditions": []
        })
    );
}
