fn main() {
    tonic_prost_build::configure()
        .build_server(true)
        .build_client(true)
        .compile_protos(&["proto/kuberic.proto"], &["proto"])
        .expect("Failed to compile proto files");
}
