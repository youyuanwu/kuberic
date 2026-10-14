fn main() {
    println!("cargo:rerun-if-changed=proto/kuberic.proto");
    println!("cargo:rerun-if-changed=proto/replication.proto");
    println!("cargo:rerun-if-env-changed=KUBERIC_WORKSPACE_TESTS");
    println!("cargo:rustc-check-cfg=cfg(kuberic_workspace_tests)");
    if std::env::var_os("KUBERIC_WORKSPACE_TESTS").is_some() {
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
}
