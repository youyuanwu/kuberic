fn main() {
    tonic_prost_build::configure()
        .build_server(true)
        .build_client(true)
        .boxed(
            ".kuberic.replication.v1.ExecuteCorrelatedControlActionRequest.action.add_replica_intent",
        )
        .compile_protos(&["proto/kuberic.proto"], &["proto"])
        .expect("Failed to compile proto files");
}
