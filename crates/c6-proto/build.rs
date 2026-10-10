fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Vendored protoc so builds need no system protobuf install.
    let protoc = protoc_bin_vendored::protoc_bin_path()?;
    // SAFETY: build scripts are single-threaded at this point.
    unsafe { std::env::set_var("PROTOC", protoc) };

    println!("cargo:rerun-if-changed=../../proto");
    tonic_prost_build::configure()
        .compile_protos(&["../../proto/carbon6/v1/hub.proto", "../../proto/carbon6/v1/session.proto"], &["../../proto"])?;
    Ok(())
}
