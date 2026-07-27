fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("cargo:rerun-if-changed=proto/csi.proto");
    // Use a vendored protoc so building does not depend on a system
    // protobuf-compiler package being present.
    unsafe {
        std::env::set_var("PROTOC", protoc_bin_vendored::protoc_bin_path()?);
    }
    // Only the Identity and Node services are implemented: this is a
    // node-local driver with no controller, so kubelet never calls
    // CreateVolume and friends.
    tonic_prost_build::configure()
        // The client is only used by the integration test that drives the
        // plugin over a real socket; kubelet is the production client.
        .build_client(true)
        .build_server(true)
        .compile_protos(&["proto/csi.proto"], &["proto"])?;
    Ok(())
}
