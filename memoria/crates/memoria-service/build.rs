fn main() -> Result<(), Box<dyn std::error::Error>> {
    let proto = "../../proto/memoria/plugin/v1/strategy.proto";
    let include_dir = "../../proto";
    let protoc = protoc_bin_vendored::protoc_bin_path()?;
    std::env::set_var("PROTOC", protoc);

    tonic_build::configure()
        .build_client(true)
        .build_server(true)
        .compile_protos(&[proto], &[include_dir])?;

    println!("cargo:rerun-if-changed={proto}");
    println!("cargo:rerun-if-changed={include_dir}");
    Ok(())
}
