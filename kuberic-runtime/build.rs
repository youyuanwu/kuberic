fn main() {
    println!("cargo:rerun-if-changed=proto/kuberic.proto");
    println!("cargo:rerun-if-changed=proto/replication.proto");
    println!("cargo:rerun-if-changed=proto/sf_v1.proto");
    println!("cargo:rerun-if-env-changed=KUBERIC_WORKSPACE_TESTS");
    println!("cargo:rustc-check-cfg=cfg(kuberic_workspace_tests)");
    if std::env::var_os("KUBERIC_WORKSPACE_TESTS").is_some() {
        let manifest = std::path::PathBuf::from(
            std::env::var_os("CARGO_MANIFEST_DIR").expect("Cargo supplies the manifest directory"),
        );
        let workspace = manifest.parent().expect("runtime has a parent directory");
        for relative in [
            "Cargo.toml",
            ".cargo/config.toml",
            "kuberic-controller/src/lib.rs",
            "kuberic-controller/src/public_v1_registration.rs",
        ] {
            assert!(
                workspace.join(relative).is_file(),
                "KUBERIC_WORKSPACE_TESTS requests repository conformance, but {relative} is \
                 missing from {}. Unset KUBERIC_WORKSPACE_TESTS for packaged consumers.",
                workspace.display()
            );
        }
        println!("cargo:rustc-cfg=kuberic_workspace_tests");
    }

    tonic_prost_build::configure()
        .build_server(true)
        .build_client(true)
        .type_attribute(
            ".kuberic.level.v1.ExecuteCommandRequest.command",
            "#[allow(clippy::large_enum_variant)]",
        )
        .boxed(".kuberic.level.v1.ScaleUpConfigurationEvidence.evidence.admission")
        .boxed(".kuberic.level.v1.ScaleUpConfigurationEvidence.evidence.failover")
        .compile_protos(
            &["proto/kuberic.proto", "proto/replication.proto"],
            &["proto"],
        )
        .expect("failed to compile level-triggered Kuberic protobuf schema");

    tonic_prost_build::configure()
        .build_server(true)
        .build_client(true)
        .bytes(".kuberic.sf.v1")
        .compile_protos(&["proto/sf_v1.proto"], &["proto"])
        .expect("failed to compile private SF V1 carrier protobuf schema");
}
